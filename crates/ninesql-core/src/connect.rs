//! Connect flow, pure parts: from a saved [`Connection`] and its [`SafetyPolicy`] to the
//! driver's [`ConnectParams`] and the SSH [`TunnelSpec`] (ADR-0004, ADR-0005, ADR-0007, ADR-0011,
//! ADR-0016).
//!
//! Secrets (I1): saved secrets are read from [`SecretStores`] (blocking: the session manager
//! runs [`resolve_secrets_bounded`] on the runtime's blocking pool, with a timeout), secrets of
//! [`SecretSource::Prompt`] fields come from the UI ([`SecretPrompt`]). They travel as
//! [`SecretString`] (zeroized on drop) in a [`SecretBag`], are moved one by one into the
//! [`HopAuth`]s and the driver's [`Secret`]s, and the bag is dropped before the driver connects.
//! Converting a [`SecretString`] into a driver [`Secret`] copies the value into a new zeroizing
//! buffer and drops (zeroizes) the original at once.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ninesql_driver_api::{self as api, ConnectParams, PropertyValue, Secret};
use ninesql_secrets::{
    ExposeSecret, SecretError, SecretKind, SecretRef, SecretStores, SecretString, StoreKind,
};
use ninesql_ssh::{HopAuth, HopSpec, Tunnel, TunnelOptions, TunnelSpec};
use tokio::sync::watch;

use crate::connection::{
    Auth, ClientCertificate, Connection, Endpoint, SecretSlot, SecretSource, SshAuth, SshHop,
    SshTunnel,
};
use crate::connection_type::SafetyPolicy;
use crate::error::{ConnectionError, CoreError};
use crate::registry::{api_engine, api_tls_mode};
use crate::session::ConfirmReason;

/// Whether a statement of `class` needs the user's confirmation under `policy` (ADR-0007 §1,
/// §3): destructive, access-mode changing or unrecognised statements when the type confirms
/// destructive statements (always for production), every write when it confirms writes.
#[must_use]
pub fn confirm_reason(policy: &SafetyPolicy, class: api::StatementClass) -> Option<ConfirmReason> {
    if policy.confirm_destructive && class.needs_confirmation() {
        Some(ConfirmReason::Destructive)
    } else if policy.confirm_every_write && class.is_write() {
        Some(ConfirmReason::Write)
    } else {
        None
    }
}

/// An init SQL statement that needs the user's confirmation before the connection is opened.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct FlaggedInitStatement {
    /// Index in [`Connection::init_sql`].
    pub index: usize,
    /// Classification by the engine's dialect ([`api::StatementClass::Unknown`] for an entry
    /// holding several statements).
    pub class: api::StatementClass,
    /// Why it needs confirmation.
    pub reason: ConfirmReason,
}

/// Session settings that change how the following statements are lexed (quoting, escapes,
/// encoding): after one of them, the client-side classifier may misread what comes next.
const LEXICAL_SETTINGS: &[&str] = &[
    "sql_mode",
    "ansi_quotes",
    "standard_conforming_strings",
    "backslash_quote",
    "escape_string_warning",
    "client_encoding",
    "encoding",
    "names",
    "character set",
    "character_set",
    "charset",
    "quoted_identifier",
];

/// Whether a session statement may change the lexical mode of the session.
fn changes_lexical_mode(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    LEXICAL_SETTINGS.iter().any(|s| lower.contains(s))
}

/// The init SQL entries that need confirmation under `policy` (ADR-0007): init SQL runs on
/// every connect, after the read-only mechanism but outside the per-statement confirmation of
/// the session, and DBeaver imports carry bootstrap queries into it.
///
/// Counted as unrecognised ([`api::StatementClass::Unknown`], so flagged):
/// - an entry that the dialect splits into several statements;
/// - a session statement that may change the lexical mode (`sql_mode`, `ANSI_QUOTES`,
///   `standard_conforming_strings`, `client_encoding`, `NAMES`, `CHARACTER SET`…), since the
///   classifier lexes with the default rules;
/// - every entry after the first session statement: once a setting ran, the classifier can no
///   longer vouch for how the server reads what follows.
///
/// The classification uses the engine's default lexical rules, not the server's `sql_mode` or
/// `standard_conforming_strings` (docs/02, "Confirmations"); the rules above and the drivers
/// (split with the session's dialect, one statement per call, read-only re-asserted) cover the
/// gap.
///
/// Imported init SQL not confirmed yet ([`Connection::init_sql_needs_confirmation`]): every
/// entry is flagged, whatever the type, with [`ConfirmReason::Imported`] unless the type's own
/// rule already flags it.
#[must_use]
pub fn init_sql_needing_confirmation(
    connection: &Connection,
    policy: &SafetyPolicy,
    dialect: &dyn api::Dialect,
) -> Vec<FlaggedInitStatement> {
    let imported = connection.init_sql_needs_confirmation;
    if !policy.confirm_destructive && !policy.confirm_every_write && !imported {
        return Vec::new();
    }
    let mut after_setting = false;
    let mut out = Vec::new();
    for (index, text) in connection.init_sql.iter().enumerate() {
        let class = if after_setting || dialect.split_statements(text).len() > 1 {
            api::StatementClass::Unknown
        } else {
            match dialect.classify(text) {
                api::StatementClass::Session if changes_lexical_mode(text) => {
                    api::StatementClass::Unknown
                }
                c => c,
            }
        };
        if matches!(
            class,
            api::StatementClass::Session
                | api::StatementClass::ChangesAccessMode
                | api::StatementClass::Unknown
        ) {
            after_setting = true;
        }
        let reason = confirm_reason(policy, class).or(imported.then_some(ConfirmReason::Imported));
        if let Some(reason) = reason {
            out.push(FlaggedInitStatement {
                index,
                class,
                reason,
            });
        }
    }
    out
}

/// A credential the UI must ask for: a [`SecretSource::Prompt`] field (ADR-0005 §3, "ask every
/// time"). The UI shows `label`, reads the value into a [`SecretString`] and opens the
/// connection again with it ([`crate::session::OpenRequest::with_secret`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretPrompt {
    /// Field the value is for.
    pub slot: SecretSlot,
    /// What kind of secret it is.
    pub kind: SecretKind,
    /// What to show, such as `Password of app on db.example.test`. Never contains a secret.
    pub label: String,
}

/// Every credential field of a connection, saved or prompted, in a stable order (database auth,
/// SSH jump hosts in order, SSH server, TLS client key).
#[must_use]
pub fn credential_fields(connection: &Connection) -> Vec<(SecretSlot, SecretKind, &SecretSource)> {
    let mut out = Vec::new();
    match &connection.auth {
        Auth::Password { secret, .. } => out.push((SecretSlot::Auth, SecretKind::Password, secret)),
        Auth::Token { secret, .. } => out.push((SecretSlot::Auth, SecretKind::Token, secret)),
        Auth::None | Auth::Username { .. } => {}
    }
    if let Some(ssh) = &connection.ssh {
        let hops = ssh
            .jump_hosts
            .iter()
            .enumerate()
            .map(|(i, h)| (SecretSlot::SshJump(i), h))
            .chain(std::iter::once((SecretSlot::SshServer, &ssh.server)));
        for (slot, hop) in hops {
            match &hop.auth {
                SshAuth::Password { secret } => out.push((slot, SecretKind::SshPassword, secret)),
                SshAuth::PublicKey {
                    passphrase: Some(secret),
                    ..
                } => out.push((slot, SecretKind::SshKeyPassphrase, secret)),
                SshAuth::PublicKey {
                    passphrase: None, ..
                }
                | SshAuth::Agent => {}
            }
        }
    }
    if let Some(ClientCertificate {
        passphrase: Some(secret),
        ..
    }) = &connection.tls.client_certificate
    {
        out.push((
            SecretSlot::TlsClientKey,
            SecretKind::ClientKeyPassphrase,
            secret,
        ));
    }
    out
}

fn prompt_label(connection: &Connection, slot: SecretSlot, kind: SecretKind) -> String {
    let hop = |h: &SshHop| format!("{} on {}", h.username, h.host);
    match slot {
        SecretSlot::Auth => {
            let what = if kind == SecretKind::Token {
                "Token"
            } else {
                "Password"
            };
            match connection.auth.username() {
                Some(user) => format!("{what} of {user} for {}", connection.name),
                None => format!("{what} for {}", connection.name),
            }
        }
        SecretSlot::SshServer | SecretSlot::SshJump(_) => {
            let h = connection.ssh.as_ref().and_then(|ssh| match slot {
                SecretSlot::SshJump(i) => ssh.jump_hosts.get(i),
                _ => Some(&ssh.server),
            });
            let what = if kind == SecretKind::SshKeyPassphrase {
                "SSH key passphrase"
            } else {
                "SSH password"
            };
            match h {
                Some(h) => format!("{what} of {}", hop(h)),
                None => what.to_owned(),
            }
        }
        SecretSlot::TlsClientKey => format!("TLS client key passphrase for {}", connection.name),
    }
}

/// The prompts still needed: every [`SecretSource::Prompt`] field without a value in `supplied`.
#[must_use]
pub fn missing_prompts(connection: &Connection, supplied: &[SecretSlot]) -> Vec<SecretPrompt> {
    credential_fields(connection)
        .into_iter()
        .filter(|(slot, _, source)| {
            matches!(source, SecretSource::Prompt) && !supplied.contains(slot)
        })
        .map(|(slot, kind, _)| SecretPrompt {
            slot,
            kind,
            label: prompt_label(connection, slot, kind),
        })
        .collect()
}

/// The resolved secrets of one connect, by field. Values are zeroized when taken out and
/// dropped, or when the bag is dropped. `Debug` lists the fields only.
#[derive(Default)]
pub struct SecretBag {
    secrets: HashMap<SecretSlot, SecretString>,
}

impl fmt::Debug for SecretBag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretBag")
            .field("slots", &self.secrets.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl SecretBag {
    /// Number of secrets held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.secrets.len()
    }

    /// Whether the bag is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.secrets.is_empty()
    }

    /// Whether a secret is held for `slot`.
    #[must_use]
    pub fn contains(&self, slot: SecretSlot) -> bool {
        self.secrets.contains_key(&slot)
    }

    /// Takes the secret of a field out of the bag.
    ///
    /// # Errors
    /// [`ConnectionError::NoSuchSecretSlot`] when none is held (a field without credential, or
    /// a prompt the UI did not answer).
    pub fn take(&mut self, slot: SecretSlot) -> Result<SecretString, ConnectionError> {
        self.secrets
            .remove(&slot)
            .ok_or(ConnectionError::NoSuchSecretSlot(slot))
    }

    /// Takes a secret as a driver [`Secret`] (the [`SecretString`] is zeroized on the way).
    fn take_driver(&mut self, slot: SecretSlot) -> Result<Secret, ConnectionError> {
        let s = self.take(slot)?;
        Ok(Secret::new(s.expose_secret().to_owned()))
    }
}

/// Resolves every credential of `connection`: saved ones through `stores`, prompted ones from
/// `prompted` (values for other fields are dropped). **Blocking** (secret store calls): run it
/// on a blocking thread.
///
/// # Errors
/// [`CoreError::Secret`] when a saved secret cannot be read;
/// [`CoreError::InvalidConnection`] with [`ConnectionError::NoSuchSecretSlot`] when a prompted
/// field has no value (call [`missing_prompts`] first).
pub fn resolve_secrets(
    connection: &Connection,
    stores: &SecretStores,
    prompted: Vec<(SecretSlot, SecretString)>,
) -> Result<SecretBag, CoreError> {
    let mut prompted: HashMap<SecretSlot, SecretString> = prompted.into_iter().collect();
    let mut bag = SecretBag::default();
    for (slot, _, source) in credential_fields(connection) {
        let value = match source {
            SecretSource::Stored(reference) => stores
                .resolve(reference)
                .map_err(|source| CoreError::Secret { slot, source })?,
            SecretSource::Prompt => prompted
                .remove(&slot)
                .ok_or(ConnectionError::NoSuchSecretSlot(slot))?,
        };
        bag.secrets.insert(slot, value);
    }
    // Values supplied for fields that do not prompt are dropped (zeroized) here.
    drop(prompted);
    Ok(bag)
}

/// Default time a connect waits for the saved secrets of one open (all store calls together)
/// before giving up: long enough for a slow keyring, short enough that an open does not hang.
pub const DEFAULT_SECRET_TIMEOUT: Duration = Duration::from_secs(60);

/// Most store reads allowed to run (or hang) at once on the blocking pool. Further reads of
/// other secrets fail with [`SecretError::Unavailable`] until some end, so a store that stops
/// answering cannot take every thread of the runtime's blocking pool.
pub const MAX_PENDING_SECRET_READS: usize = 8;

type ReadOutcome = Option<Result<SecretString, SecretError>>;

/// Saved-secret reads of a [`crate::SessionManager`]: the stores, and the reads in flight.
///
/// Reads are **single-flight** per [`SecretRef`]: an open that needs a secret already being read
/// (for example a retry after a timeout, while the first call still waits on the store) joins
/// that read instead of starting another store call. At most [`MAX_PENDING_SECRET_READS`] reads
/// run at once.
#[derive(Clone)]
pub struct SecretReads {
    stores: SecretStores,
    flights: Arc<Mutex<Flights>>,
    max_pending: usize,
}

#[derive(Default)]
struct Flights {
    reads: HashMap<SecretRef, watch::Receiver<ReadOutcome>>,
}

impl fmt::Debug for SecretReads {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretReads")
            .field("stores", &self.stores)
            .field("pending", &self.pending())
            .finish()
    }
}

/// Removes a read from the in-flight list when its blocking task ends, panic included.
struct FlightGuard {
    flights: Arc<Mutex<Flights>>,
    key: SecretRef,
}

impl Drop for FlightGuard {
    fn drop(&mut self) {
        lock_flights(&self.flights).reads.remove(&self.key);
    }
}

fn lock_flights(flights: &Mutex<Flights>) -> std::sync::MutexGuard<'_, Flights> {
    flights
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A copy of a read's outcome for one more waiter. Store errors carry no secret; variants added
/// later are reported as a backend failure with the same message.
fn copy_outcome(
    outcome: &Result<SecretString, SecretError>,
    store: StoreKind,
) -> Result<SecretString, SecretError> {
    match outcome {
        Ok(s) => Ok(SecretString::from(s.expose_secret().to_owned())),
        Err(e) => Err(match e {
            SecretError::NotFound(id) => SecretError::NotFound(id.clone()),
            SecretError::StoreNotConfigured(k) => SecretError::StoreNotConfigured(*k),
            SecretError::Locked(k) => SecretError::Locked(*k),
            SecretError::Unavailable { store, detail } => SecretError::Unavailable {
                store: *store,
                detail: detail.clone(),
            },
            SecretError::BadEncoding(id) => SecretError::BadEncoding(id.clone()),
            SecretError::Ambiguous(id) => SecretError::Ambiguous(id.clone()),
            SecretError::Backend { store, detail } => SecretError::Backend {
                store: *store,
                detail: detail.clone(),
            },
            other => SecretError::Backend {
                store,
                detail: other.to_string(),
            },
        }),
    }
}

impl SecretReads {
    /// Reads through `stores`, at most [`MAX_PENDING_SECRET_READS`] at once.
    #[must_use]
    pub fn new(stores: SecretStores) -> Self {
        Self {
            stores,
            flights: Arc::default(),
            max_pending: MAX_PENDING_SECRET_READS,
        }
    }

    /// The stores.
    #[must_use]
    pub fn stores(&self) -> &SecretStores {
        &self.stores
    }

    /// Reads running (or hanging) on the blocking pool now.
    #[must_use]
    pub fn pending(&self) -> usize {
        lock_flights(&self.flights).reads.len()
    }

    /// Joins the read of `reference` in flight, or starts it. Must run inside the runtime.
    fn flight(&self, reference: &SecretRef) -> Result<watch::Receiver<ReadOutcome>, SecretError> {
        let mut flights = lock_flights(&self.flights);
        if let Some(rx) = flights.reads.get(reference) {
            return Ok(rx.clone());
        }
        if flights.reads.len() >= self.max_pending {
            return Err(SecretError::Unavailable {
                store: reference.store,
                detail: "too many secret store calls are still waiting for an answer".into(),
            });
        }
        let (tx, rx) = watch::channel(None);
        flights.reads.insert(reference.clone(), rx.clone());
        drop(flights);
        let guard = FlightGuard {
            flights: Arc::clone(&self.flights),
            key: reference.clone(),
        };
        let stores = self.stores.clone();
        let reference = reference.clone();
        // Detached: when every waiter gave up, the call still ends on the store's own limits and
        // its value is dropped (zeroized) with the channel.
        drop(tokio::task::spawn_blocking(move || {
            let _guard = guard;
            let result = stores.resolve(&reference);
            let _ = tx.send(Some(result));
        }));
        Ok(rx)
    }

    /// Reads one saved secret, waiting at most until `deadline`.
    async fn read(
        &self,
        reference: &SecretRef,
        deadline: tokio::time::Instant,
        timeout: Duration,
    ) -> Result<SecretString, SecretError> {
        let store = reference.store;
        let mut rx = self.flight(reference)?;
        let wait = async {
            loop {
                if let Some(outcome) = rx.borrow_and_update().as_ref() {
                    return Some(copy_outcome(outcome, store));
                }
                if rx.changed().await.is_err() {
                    // The task ended: with a value sent just before, or by a panic.
                    return rx.borrow().as_ref().map(|o| copy_outcome(o, store));
                }
            }
        };
        match tokio::time::timeout_at(deadline, wait).await {
            Ok(Some(result)) => result,
            Ok(None) => {
                tracing::warn!(store = %store, "secret store task failed");
                Err(SecretError::Unavailable {
                    store,
                    detail: "the secret store task failed".into(),
                })
            }
            Err(_) => {
                tracing::info!(store = %store, "secret store did not answer in time");
                Err(SecretError::Unavailable {
                    store,
                    detail: format!("no answer within {} s", timeout.as_secs()),
                })
            }
        }
    }
}

/// [`resolve_secrets`] for the async connect flow: saved secrets are read on the runtime's
/// blocking pool through `reads` (single-flight per reference, bounded number of pending
/// reads), and all of them together get at most `timeout`.
///
/// When the deadline passes, the blocking task panics or too many reads are pending, the field
/// fails with [`SecretError::Unavailable`]. The future is cancel-safe: dropping it stops waiting
/// at once. A store call already running on a blocking thread cannot be interrupted: it ends on
/// the store's own limits (the Secret Service read path never waits on a prompt, and each of its
/// D-Bus calls has a 25 s timeout) and its value, if nobody waits for it any more, is dropped
/// (zeroized) unused. A later open of the same secret joins it while it runs.
///
/// # Errors
/// As [`resolve_secrets`].
pub async fn resolve_secrets_bounded(
    connection: &Connection,
    reads: &SecretReads,
    prompted: Vec<(SecretSlot, SecretString)>,
    timeout: Duration,
) -> Result<SecretBag, CoreError> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut prompted: HashMap<SecretSlot, SecretString> = prompted.into_iter().collect();
    let mut bag = SecretBag::default();
    for (slot, _, source) in credential_fields(connection) {
        let value = match source {
            SecretSource::Stored(reference) => reads
                .read(reference, deadline, timeout)
                .await
                .map_err(|source| CoreError::Secret { slot, source })?,
            SecretSource::Prompt => prompted
                .remove(&slot)
                .ok_or(ConnectionError::NoSuchSecretSlot(slot))?,
        };
        bag.secrets.insert(slot, value);
    }
    drop(prompted);
    Ok(bag)
}

/// Default port of an engine, when it has a usual one. `None` means the connection must name a
/// port.
#[must_use]
pub fn default_port(engine: &str) -> Option<u16> {
    Some(match engine {
        "postgresql" | "timescaledb" => 5432,
        "yugabytedb" => 5433,
        "questdb" => 8812,
        "mysql" | "mariadb" => 3306,
        "tidb" => 4000,
        "clickhouse" => 8123,
        "trino" => 8080,
        "firebird" => 3050,
        "influxdb" => 8181,
        "mongodb" => 27017,
        "redis" | "valkey" => 6379,
        "elasticsearch" | "opensearch" => 9200,
        "cassandra" => 9042,
        "neo4j" => 7687,
        _ => return None,
    })
}

/// Where the driver must connect, before any tunnel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A TCP server: real host and port (the tunnel's target when there is a tunnel).
    Tcp {
        /// Database host.
        host: String,
        /// Port (explicit or the engine's default).
        port: u16,
        /// Database to open.
        database: Option<String>,
    },
    /// A database file.
    File(std::path::PathBuf),
}

/// The connection's endpoint as a [`Target`].
///
/// # Errors
/// [`CoreError::UnsupportedEndpoint`] for Unix sockets (no driver API endpoint yet) and for an
/// SSH tunnel to a non-TCP endpoint; [`CoreError::PortRequired`].
pub fn target(connection: &Connection) -> Result<Target, CoreError> {
    let target = match &connection.endpoint {
        Endpoint::Tcp {
            host,
            port,
            database,
        } => {
            let port = match port {
                Some(p) => *p,
                None => default_port(connection.engine.as_str())
                    .ok_or_else(|| CoreError::PortRequired(connection.engine.to_string()))?,
            };
            Target::Tcp {
                host: host.clone(),
                port,
                database: database.clone(),
            }
        }
        Endpoint::UnixSocket { .. } => {
            return Err(CoreError::UnsupportedEndpoint(
                "Unix socket endpoints are not supported by the drivers yet; use a TCP endpoint",
            ));
        }
        Endpoint::File { path } => Target::File(path.clone()),
    };
    if connection.ssh.is_some() && !matches!(target, Target::Tcp { .. }) {
        return Err(CoreError::UnsupportedEndpoint(
            "an SSH tunnel needs a TCP endpoint",
        ));
    }
    Ok(target)
}

fn hop_spec(hop: &SshHop, slot: SecretSlot, bag: &mut SecretBag) -> Result<HopSpec, CoreError> {
    let auth = match &hop.auth {
        SshAuth::Agent => HopAuth::Agent,
        SshAuth::Password { .. } => HopAuth::Password(bag.take(slot)?),
        SshAuth::PublicKey {
            key_path,
            passphrase,
        } => HopAuth::PublicKey {
            key_path: key_path.clone(),
            passphrase: match passphrase {
                Some(_) => Some(bag.take(slot)?),
                None => None,
            },
        },
    };
    Ok(HopSpec::new(hop.host.clone(), hop.username.clone(), auth)
        .with_port(hop.port)
        .with_host_key_policy(hop.host_key_policy.to_ssh()))
}

/// The [`TunnelSpec`] of a connection's SSH tunnel to `target_host:target_port`, each hop with
/// its own host key policy (ADR-0016). SSH secrets are taken out of `bag`.
///
/// # Errors
/// [`CoreError::InvalidConnection`] when a hop's secret is missing from the bag.
pub fn tunnel_spec(
    ssh: &SshTunnel,
    bag: &mut SecretBag,
    target_host: &str,
    target_port: u16,
    connect_timeout: Duration,
) -> Result<TunnelSpec, CoreError> {
    let server = hop_spec(&ssh.server, SecretSlot::SshServer, bag)?;
    let mut options = TunnelOptions::default();
    options.connect_timeout = connect_timeout;
    let mut spec = TunnelSpec::new(server, target_host, target_port).with_options(options);
    for (i, jump) in ssh.jump_hosts.iter().enumerate() {
        spec = spec.via(hop_spec(jump, SecretSlot::SshJump(i), bag)?);
    }
    Ok(spec)
}

/// Builds the driver's parameters. Database and TLS secrets are moved out of `bag`.
///
/// - endpoint: the tunnel's local end when `tunnel` is set, the real host otherwise; with a
///   tunnel, `tls.server_name` is the real database host, so `verify-full` still checks the
///   server's name (ADR-0011);
/// - read-only, auto-commit, production and statement logging (`log_query_text`) come from
///   `policy` (I3, ADR-0007);
/// - the password exchange policy comes from [`Connection::password_exchange`];
/// - `inside_dbeaver_workspace` is left `false`: it needs the file system, so the session
///   manager sets it from its [`crate::DbeaverRoots`] ([`crate::DbeaverRoots::contains`]) on
///   the blocking pool;
/// - init SQL and driver properties as saved (validated by
///   [`Connection::validate_for_driver`] before).
///
/// # Errors
/// [`CoreError::InvalidConnection`] when a secret is missing from the bag;
/// [`CoreError::UnsupportedEndpoint`] / [`CoreError::PortRequired`] (see [`target`]).
pub fn connect_params(
    connection: &Connection,
    policy: &SafetyPolicy,
    bag: &mut SecretBag,
    tunnel: Option<&Tunnel>,
    connect_timeout: Duration,
) -> Result<ConnectParams, CoreError> {
    let engine = api_engine(&connection.engine);
    let (endpoint, database, real_host) = match target(connection)? {
        Target::Tcp {
            host,
            port,
            database,
        } => match tunnel {
            Some(t) => (t.endpoint(), database, Some(host)),
            None => (api::Endpoint::tcp(host, port), database, None),
        },
        Target::File(path) => (api::Endpoint::file(path), None, None),
    };
    let mut params = ConnectParams::new(engine, endpoint);
    params.database = database;
    params.auth = match &connection.auth {
        Auth::None => api::Auth::None,
        Auth::Username { username } => api::Auth::password(username.clone(), None),
        Auth::Password { username, .. } => {
            api::Auth::password(username.clone(), Some(bag.take_driver(SecretSlot::Auth)?))
        }
        Auth::Token { username, .. } => {
            api::Auth::token(username.clone(), bag.take_driver(SecretSlot::Auth)?)
        }
    };
    let mut tls = api::TlsConfig::default();
    tls.mode = api_tls_mode(connection.tls.mode);
    tls.ca_file.clone_from(&connection.tls.ca_file);
    if let Some(cert) = &connection.tls.client_certificate {
        let mut client = api::ClientCertificate::new(&cert.cert_file, &cert.key_file);
        if cert.passphrase.is_some() {
            client.key_passphrase = Some(bag.take_driver(SecretSlot::TlsClientKey)?);
        }
        tls.client_certificate = Some(client);
    }
    tls.server_name = real_host;
    params.tls = tls;
    params.tunnel = tunnel.map(|t| t.info().clone());
    params.password_exchange = connection.password_exchange.to_api();
    params.read_only = policy.read_only;
    params.auto_commit = policy.auto_commit;
    params.production = policy.production;
    // L1: a custom type's `log_query_text = false` reaches the driver, not only `production`.
    params.log_statements = policy.log_query_text;
    params.connect_timeout = connect_timeout;
    params.init_statements.clone_from(&connection.init_sql);
    params.properties = connection
        .properties
        .iter()
        .map(|(k, v)| (k.clone(), PropertyValue::Plain(v.clone())))
        .collect();
    Ok(params)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).
mod tests {
    use std::sync::Arc;

    use ninesql_secrets::{MemoryStore, SecretId, SecretMeta, SecretRef, StoreKind};

    use super::*;
    use crate::connection::{EngineId, HostKeyPolicy};
    use crate::connection_type::{ConnectionTypeId, ConnectionTypes};

    fn conn() -> Connection {
        Connection::new(
            "Orders",
            EngineId::new("postgresql").unwrap(),
            Endpoint::Tcp {
                host: "db.example.test".into(),
                port: None,
                database: Some("orders".into()),
            },
            Auth::Password {
                username: "app".into(),
                secret: SecretSource::Prompt,
            },
            ConnectionTypeId::prod(),
        )
    }

    fn with_ssh(mut c: Connection) -> Connection {
        let mut jump = SshHop::new(
            "edge.example.test",
            2222,
            "jump",
            SshAuth::PublicKey {
                key_path: "/k".into(),
                passphrase: Some(SecretSource::Prompt),
            },
        );
        jump.host_key_policy = HostKeyPolicy::AcceptNew;
        c.ssh = Some(SshTunnel {
            server: SshHop::new(
                "bastion.example.test",
                22,
                "deploy",
                SshAuth::Password {
                    secret: SecretSource::Prompt,
                },
            ),
            jump_hosts: vec![jump],
        });
        c
    }

    fn s(v: &str) -> SecretString {
        SecretString::from(v.to_owned())
    }

    #[test]
    fn prompts_cover_every_unsaved_field_in_order() {
        let c = with_ssh(conn());
        let p = missing_prompts(&c, &[]);
        let slots: Vec<_> = p.iter().map(|p| p.slot).collect();
        assert_eq!(
            slots,
            [
                SecretSlot::Auth,
                SecretSlot::SshJump(0),
                SecretSlot::SshServer
            ]
        );
        assert_eq!(p[0].label, "Password of app for Orders");
        assert_eq!(
            p[1].label,
            "SSH key passphrase of jump on edge.example.test"
        );
        assert!(missing_prompts(&c, &slots).is_empty());
    }

    #[test]
    fn saved_and_prompted_secrets_resolve_and_extras_are_dropped() {
        let keyring = Arc::new(MemoryStore::new(StoreKind::Keyring));
        let mut stores = SecretStores::new();
        stores.register(keyring.clone());
        let mut c = conn();
        let r = SecretRef::new(StoreKind::Keyring, SecretId::new("c:auth").unwrap());
        stores
            .save(
                &r,
                &SecretMeta {
                    connection_id: "c".into(),
                    kind: SecretKind::Password,
                    label: "x".into(),
                },
                &s("saved-pw"),
            )
            .unwrap();
        c.set_secret_ref(SecretSlot::Auth, r).unwrap();
        // A value for a field that does not prompt is ignored.
        let mut bag =
            resolve_secrets(&c, &stores, vec![(SecretSlot::TlsClientKey, s("extra"))]).unwrap();
        assert_eq!(bag.len(), 1);
        assert!(!format!("{bag:?}").contains("saved-pw"));
        assert_eq!(
            bag.take(SecretSlot::Auth).unwrap().expose_secret(),
            "saved-pw"
        );
        assert!(bag.is_empty());

        // A missing saved secret is a typed error naming the field.
        let empty = SecretStores::new();
        assert!(matches!(
            resolve_secrets(&c, &empty, vec![]),
            Err(CoreError::Secret {
                slot: SecretSlot::Auth,
                ..
            })
        ));
        // An unanswered prompt.
        assert!(matches!(
            resolve_secrets(&conn(), &stores, vec![]),
            Err(CoreError::InvalidConnection(
                ConnectionError::NoSuchSecretSlot(SecretSlot::Auth)
            ))
        ));
    }

    /// L1: a custom, non-production type with `log_query_text = false` turns statement
    /// logging off in the driver; the development type keeps it on.
    #[test]
    fn statement_logging_follows_the_connection_type() {
        use crate::connection_type::ConnectionType;
        let mut types = ConnectionTypes::default();
        let mut quiet = ConnectionType::development();
        quiet.id = ConnectionTypeId::new("quiet").unwrap();
        quiet.log_query_text = false;
        types.upsert(quiet);
        for (id, logs) in [("dev", true), ("quiet", false)] {
            let mut c = conn();
            c.connection_type = ConnectionTypeId::new(id).unwrap();
            let policy = c.policy(&types);
            assert!(!policy.production, "{id}");
            let mut bag =
                resolve_secrets(&c, &SecretStores::new(), vec![(SecretSlot::Auth, s("pw"))])
                    .unwrap();
            let p = connect_params(&c, &policy, &mut bag, None, Duration::from_secs(3)).unwrap();
            assert_eq!(p.log_statements, logs, "{id}");
            assert_eq!(p.may_log_statements(), logs, "{id}");
        }
    }

    #[test]
    fn params_follow_policy_and_take_the_secrets() {
        let types = ConnectionTypes::default();
        let mut c = conn();
        c.init_sql = vec!["SET search_path TO app".into()];
        c.properties.insert("application_name".into(), "x".into());
        let policy = c.policy(&types);
        let mut bag =
            resolve_secrets(&c, &SecretStores::new(), vec![(SecretSlot::Auth, s("pw"))]).unwrap();
        let p = connect_params(&c, &policy, &mut bag, None, Duration::from_secs(3)).unwrap();
        assert!(bag.is_empty(), "the secret was moved into the params");
        assert!(p.production);
        assert!(!p.log_statements && !p.may_log_statements());
        assert!(!p.auto_commit);
        assert_eq!(
            p.endpoint,
            api::Endpoint::tcp("db.example.test", 5432),
            "default port"
        );
        assert_eq!(p.database.as_deref(), Some("orders"));
        assert_eq!(p.tls.mode, api::TlsMode::VerifyFull);
        assert_eq!(p.tls.server_name, None);
        assert_eq!(p.init_statements.len(), 1);
        assert_eq!(p.connect_timeout, Duration::from_secs(3));
        assert!(!format!("{p:?}").contains("pw\""));
        match &p.auth {
            api::Auth::Password { user, password, .. } => {
                assert_eq!(user, "app");
                assert_eq!(password.as_ref().unwrap().expose(), "pw");
            }
            other => panic!("{other:?}"),
        }

        c.read_only = Some(true);
        let policy = c.policy(&types);
        let mut bag =
            resolve_secrets(&c, &SecretStores::new(), vec![(SecretSlot::Auth, s("pw"))]).unwrap();
        assert!(
            connect_params(&c, &policy, &mut bag, None, Duration::from_secs(3))
                .unwrap()
                .read_only
        );
    }

    #[test]
    fn endpoints() {
        let mut c = conn();
        c.engine = EngineId::new("exotic").unwrap();
        assert!(matches!(target(&c), Err(CoreError::PortRequired(_))));
        c.endpoint = Endpoint::UnixSocket {
            path: "/run/pg".into(),
            database: None,
        };
        assert!(matches!(target(&c), Err(CoreError::UnsupportedEndpoint(_))));
        c.endpoint = Endpoint::File {
            path: "/tmp/x.db".into(),
        };
        assert_eq!(target(&c).unwrap(), Target::File("/tmp/x.db".into()));
        let c = with_ssh(c);
        assert!(matches!(target(&c), Err(CoreError::UnsupportedEndpoint(_))));
    }

    #[test]
    fn tunnel_spec_keeps_per_hop_policies_and_takes_ssh_secrets() {
        let c = with_ssh(conn());
        let mut bag = resolve_secrets(
            &c,
            &SecretStores::new(),
            vec![
                (SecretSlot::Auth, s("db")),
                (SecretSlot::SshServer, s("ssh")),
                (SecretSlot::SshJump(0), s("pass")),
            ],
        )
        .unwrap();
        let spec = tunnel_spec(
            c.ssh.as_ref().unwrap(),
            &mut bag,
            "db.example.test",
            5432,
            Duration::from_secs(7),
        )
        .unwrap();
        assert!(bag.contains(SecretSlot::Auth));
        assert!(!bag.contains(SecretSlot::SshServer));
        assert!(!bag.contains(SecretSlot::SshJump(0)));
        assert_eq!(spec.target_host, "db.example.test");
        assert_eq!(spec.target_port, 5432);
        assert_eq!(spec.options.connect_timeout, Duration::from_secs(7));
        assert_eq!(
            spec.server.host_key_policy,
            ninesql_ssh::HostKeyPolicy::Strict
        );
        assert_eq!(spec.jump_hosts.len(), 1);
        assert_eq!(spec.jump_hosts[0].port, 2222);
        assert_eq!(
            spec.jump_hosts[0].host_key_policy,
            ninesql_ssh::HostKeyPolicy::AcceptNew
        );
        let warnings = spec.security_warnings();
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].hop, 0);
        assert!(!format!("{spec:?}").contains("ssh\""));
    }
}
