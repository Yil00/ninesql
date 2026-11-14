//! Session manager: opens and closes driver sessions on the [`DriverRuntime`] and streams their
//! results back over channels, so the UI never awaits I/O (ADR-0004, I6).
//!
//! # Shape
//! - [`SessionManager::open`] runs the connect flow on the runtime and answers through a
//!   [`Reply`]: the session ([`OpenOutcome::Opened`]), the secrets to ask for
//!   ([`OpenOutcome::NeedsSecrets`]) or an SSH host key to confirm
//!   ([`OpenOutcome::HostKeyNeedsConfirmation`]).
//! - Each session is owned by one task. [`SessionHandle`] sends it commands over a bounded
//!   channel; [`SessionHandle::execute`] returns an [`Execution`] whose events
//!   ([`ExecEvent`]) carry the result items one by one through a small bounded channel: a slow
//!   consumer slows the fetch down instead of filling memory, on top of the stream's own
//!   budget (I6).
//! - [`Reply`] and [`Execution`] are executor-agnostic: GPUI awaits them on its own executor or
//!   polls them with `try_take` / `try_next`.
//! - Session-level events (opened, closed, tunnel lost) go to one [`CoreEvent`] channel
//!   ([`SessionManager::take_events`]).
//!
//! # Cancel and `Busy`
//! [`Execution::cancel`] (or [`SessionHandle::cancel_current`]) sets the execution's
//! [`CancelToken`]; the result stream calls the execution-scoped server-side cancel and ends.
//! The driver keeps the session busy until the server confirmed the end (M6). A statement sent
//! meanwhile does not fail with `Busy`: the session task reports
//! [`ExecEvent::WaitingForPreviousCancel`] and retries until the session is free, for at most
//! [`ManagerConfig::busy_wait`], then fails with [`CoreError::StillCancelling`].
//!
//! # Production safety (ADR-0007)
//! Before every statement the session task classifies it with the session's dialect. On a
//! connection whose type confirms destructive statements (always for production) or every
//! write, a statement that needs it is not sent: the execution ends with
//! [`ExecEvent::NeedsConfirmation`] and a single-use [`ConfirmationToken`] bound to the exact
//! text and session. The UI shows the dialog and sends the statement again with the token.
//! Query text is logged only at `trace` level and never when the type forbids it (always for
//! production).

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use ninesql_driver_api::{
    CancelToken, Capabilities, DriverError, ExecOptions, ExecutionId, FetchBudget, Param,
    QueryLanguage, ReadOnlyMechanism, ResultItem, Session, Statement, StatementClass, TunnelInfo,
};
use ninesql_secrets::{SecretStores, SecretString};
use ninesql_ssh::{
    KnownHosts, KnownHostsStore, SshError, SshSecurityWarning, Tunnel, UnknownHostKey,
};
use ninesql_storage::Paths;
use tokio::sync::{OnceCell, mpsc, oneshot};
use uuid::Uuid;

use crate::connect::{self, SecretPrompt};
use crate::connection::{Connection, ConnectionId, EngineId, SecretSlot};
use crate::connection_type::{ConnectionTypes, SafetyPolicy};
use crate::error::CoreError;
use crate::registry::{DriverRegistry, api_engine};
use crate::runtime::{DEFAULT_WORKERS, DriverRuntime};

/// Pause between two attempts while the previous execution is still being cancelled.
const BUSY_RETRY: Duration = Duration::from_millis(25);
/// Extra time given to the driver on top of its own connect timeout.
const CONNECT_MARGIN: Duration = Duration::from_secs(5);
/// Confirmation tokens kept per session.
const MAX_PENDING_CONFIRMATIONS: usize = 16;
/// Lifetime of a confirmation token.
const CONFIRMATION_TTL: Duration = Duration::from_secs(10 * 60);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // Critical sections are a few map operations without panics: the data stays consistent.
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

// ---------------------------------------------------------------------------------------------
// Replies

/// State of a non-blocking poll of a [`Reply`] or an [`Execution`].
#[derive(Debug)]
pub enum Next<T> {
    /// A value is available.
    Ready(T),
    /// Nothing yet; poll again later.
    Pending,
    /// Nothing more will come (the task ended, or the value was already taken).
    Ended,
}

/// The answer of a request run on the driver runtime. Await it from any executor (`Output` is
/// `None` when the task ended without answering, for example at shutdown), or poll it with
/// [`Reply::try_take`] without blocking.
pub struct Reply<T> {
    rx: Option<oneshot::Receiver<T>>,
}

impl<T> fmt::Debug for Reply<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Reply")
            .field("taken", &self.rx.is_none())
            .finish()
    }
}

impl<T> Reply<T> {
    fn channel() -> (oneshot::Sender<T>, Self) {
        let (tx, rx) = oneshot::channel();
        (tx, Self { rx: Some(rx) })
    }

    fn ready(value: T) -> Self {
        let (tx, reply) = Self::channel();
        let _ = tx.send(value);
        reply
    }

    /// Takes the answer if it is there. Never blocks.
    pub fn try_take(&mut self) -> Next<T> {
        let Some(rx) = self.rx.as_mut() else {
            return Next::Ended;
        };
        match rx.try_recv() {
            Ok(v) => {
                self.rx = None;
                Next::Ready(v)
            }
            Err(oneshot::error::TryRecvError::Empty) => Next::Pending,
            Err(oneshot::error::TryRecvError::Closed) => {
                self.rx = None;
                Next::Ended
            }
        }
    }
}

impl<T> Future for Reply<T> {
    type Output = Option<T>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let Some(rx) = self.rx.as_mut() else {
            return Poll::Ready(None);
        };
        match Pin::new(rx).poll(cx) {
            Poll::Ready(v) => {
                self.rx = None;
                Poll::Ready(v.ok())
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Configuration and events

/// The `known_hosts` files used for SSH host key checks (ADR-0011 §6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnownHostsFiles {
    /// NineSQL's own file (read-write): [`Paths::known_hosts_file`].
    pub own: PathBuf,
    /// The user's OpenSSH file (read-only), usually `~/.ssh/known_hosts`.
    pub user: Option<PathBuf>,
}

impl KnownHostsFiles {
    /// NineSQL's file under `paths` and the user's `~/.ssh/known_hosts`.
    #[must_use]
    pub fn new(paths: &Paths) -> Self {
        Self {
            own: paths.known_hosts_file(),
            user: KnownHosts::user_default_path(),
        }
    }
}

/// Settings of a [`SessionManager`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ManagerConfig {
    /// Worker threads of the driver runtime (ADR-0004: default 4).
    pub workers: usize,
    /// Connect timeout, per SSH hop and for the driver's connect.
    pub connect_timeout: Duration,
    /// How long a statement waits for the previous, cancelled one to be confirmed ended before
    /// failing with [`CoreError::StillCancelling`]. Default: the driver API's
    /// [`SERVER_CANCEL_GRACE`](ninesql_driver_api::SERVER_CANCEL_GRACE) plus one second.
    pub busy_wait: Duration,
    /// Result events buffered per execution before the fetch waits for the consumer.
    pub result_queue: usize,
    /// Commands queued per session.
    pub command_queue: usize,
    /// `known_hosts` files.
    pub known_hosts: KnownHostsFiles,
}

impl ManagerConfig {
    /// Defaults, with the `known_hosts` files of `paths`.
    #[must_use]
    pub fn new(paths: &Paths) -> Self {
        Self::with_known_hosts(KnownHostsFiles::new(paths))
    }

    /// Defaults, with explicit `known_hosts` files.
    #[must_use]
    pub fn with_known_hosts(known_hosts: KnownHostsFiles) -> Self {
        Self {
            workers: DEFAULT_WORKERS,
            connect_timeout: ninesql_driver_api::ConnectParams::DEFAULT_CONNECT_TIMEOUT,
            busy_wait: ninesql_driver_api::SERVER_CANCEL_GRACE + Duration::from_secs(1),
            result_queue: 4,
            command_queue: 16,
            known_hosts,
        }
    }
}

/// Identifier of an open session, unique within a [`SessionManager`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SessionId(u64);

impl SessionId {
    /// The numeric value.
    #[must_use]
    pub fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "session-{}", self.0)
    }
}

/// Why a session ended.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CloseReason {
    /// Closed on request ([`SessionHandle::close`], [`SessionManager::close_all`]).
    Requested,
    /// The SSH tunnel was lost (network, keepalive timeout, server shutdown).
    TunnelLost,
    /// The driver reported the connection unusable (for example a lifted read-only mode, I3).
    Lost(DriverError),
    /// The session task panicked; the application keeps running.
    Panicked,
}

/// Session-level event, for the UI ([`SessionManager::take_events`]).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CoreEvent {
    /// A session was opened.
    SessionOpened {
        /// Session.
        session: SessionId,
        /// Its connection.
        connection: ConnectionId,
    },
    /// A session ended; its handles now fail with [`CoreError::SessionClosed`].
    SessionClosed {
        /// Session.
        session: SessionId,
        /// Its connection.
        connection: ConnectionId,
        /// Why.
        reason: CloseReason,
    },
}

/// A weakened security setting of an open session, shown on its tabs (I3, I5).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SessionWarning {
    /// From the driver parameters: TLS downgrade, credentials in clear, an SSH hop without
    /// host key verification.
    Connection(ninesql_driver_api::SecurityWarning),
    /// From the SSH tunnel: per-hop policy downgrades and host keys recorded without
    /// confirmation while opening it (ADR-0016).
    Ssh(SshSecurityWarning),
    /// The session is read-only, but the engine has no engine-side mechanism: the driver only
    /// refuses the writes it can classify (ADR-0007 §2). The UI says so in plain words.
    ReadOnlyClientSideOnly,
}

/// What the UI needs to know about an open session. Never contains a secret.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct SessionInfo {
    /// Session.
    pub id: SessionId,
    /// Connection it was opened from.
    pub connection: ConnectionId,
    /// Connection name, for messages and confirmation dialogs.
    pub connection_name: String,
    /// Engine.
    pub engine: EngineId,
    /// Exact capabilities of the connected server (I8).
    pub capabilities: Capabilities,
    /// Server version, as reported.
    pub server_version: Option<String>,
    /// The session is read-only (I3).
    pub read_only: bool,
    /// Effective safety rules of the connection (ADR-0007).
    pub policy: SafetyPolicy,
    /// Auto-commit mode right after connect.
    pub auto_commit: bool,
    /// Route of the SSH tunnel, if any.
    pub tunnel: Option<TunnelInfo>,
    /// Weakened security settings.
    pub warnings: Vec<SessionWarning>,
}

/// Transaction state of a session after a transaction command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransactionState {
    /// Auto-commit mode.
    pub auto_commit: bool,
    /// A transaction with uncommitted work is open.
    pub in_transaction: bool,
}

// ---------------------------------------------------------------------------------------------
// Open

/// A request to open a session. The safety policy is computed from the connection types when
/// the request is built, so the UI cannot pass a weaker one. `Debug` never shows a secret.
pub struct OpenRequest {
    connection: Connection,
    policy: SafetyPolicy,
    prompted: Vec<(SecretSlot, SecretString)>,
}

impl fmt::Debug for OpenRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenRequest")
            .field("connection", &self.connection.id)
            .field("policy", &self.policy)
            .field(
                "prompted",
                &self.prompted.iter().map(|(s, _)| *s).collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl OpenRequest {
    /// Opens `connection` under the rules of its type in `types` ([`Connection::policy`]).
    #[must_use]
    pub fn new(connection: Connection, types: &ConnectionTypes) -> Self {
        let policy = connection.policy(types);
        Self {
            connection,
            policy,
            prompted: Vec::new(),
        }
    }

    /// Adds the answer to a [`SecretPrompt`]. The value stays in memory only until the connect
    /// is done (ADR-0005 §3), and is zeroized when dropped.
    #[must_use]
    pub fn with_secret(mut self, slot: SecretSlot, secret: SecretString) -> Self {
        self.prompted.retain(|(s, _)| *s != slot);
        self.prompted.push((slot, secret));
        self
    }

    /// The effective policy the session will get.
    #[must_use]
    pub fn policy(&self) -> &SafetyPolicy {
        &self.policy
    }
}

/// Result of [`SessionManager::open`].
#[derive(Debug)]
#[non_exhaustive]
pub enum OpenOutcome {
    /// The session is open.
    Opened(SessionHandle),
    /// Credentials to ask for ("ask every time" fields); open again with
    /// [`OpenRequest::with_secret`] for each. Nothing reached the network.
    NeedsSecrets(Vec<SecretPrompt>),
    /// An SSH hop presented an unknown host key under the strict policy. Show the hop, the key
    /// type and the SHA-256 fingerprint (and, when `other_key_types` is not empty, that the host
    /// is known under another key type); if the user confirms, call
    /// [`SessionManager::trust_host_key`] and open again (prompted secrets must be given again:
    /// the core does not keep them while the user decides). No credential was sent to that hop.
    HostKeyNeedsConfirmation(Box<UnknownHostKey>),
}

struct Shared {
    registry: DriverRegistry,
    stores: SecretStores,
    config: ManagerConfig,
    known_hosts: OnceCell<KnownHostsStore>,
    sessions: Mutex<HashMap<SessionId, SessionHandle>>,
    events: mpsc::UnboundedSender<CoreEvent>,
    next_session: AtomicU64,
}

impl Shared {
    async fn known_hosts(&self) -> Result<KnownHostsStore, CoreError> {
        let files = &self.config.known_hosts;
        let store = self
            .known_hosts
            .get_or_try_init(|| KnownHostsStore::load(files.own.clone(), files.user.clone()))
            .await?;
        Ok(store.clone())
    }

    fn emit(&self, event: CoreEvent) {
        // No receiver (the UI did not take the channel, or dropped it): nothing to do.
        let _ = self.events.send(event);
    }
}

/// Owns the driver runtime and every open session. Cheap to clone; dropping the last clone stops
/// the runtime in the background (sessions are dropped without a clean close: call
/// [`SessionManager::shutdown`] at exit).
#[derive(Clone)]
pub struct SessionManager {
    runtime: Arc<DriverRuntime>,
    shared: Arc<Shared>,
    events_rx: Arc<Mutex<Option<mpsc::UnboundedReceiver<CoreEvent>>>>,
}

impl fmt::Debug for SessionManager {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionManager")
            .field("registry", &self.shared.registry)
            .field("sessions", &lock(&self.shared.sessions).len())
            .finish_non_exhaustive()
    }
}

impl SessionManager {
    /// Starts the driver runtime.
    ///
    /// # Errors
    /// [`CoreError::Runtime`].
    pub fn new(
        config: ManagerConfig,
        registry: DriverRegistry,
        stores: SecretStores,
    ) -> Result<Self, CoreError> {
        let runtime = DriverRuntime::new(config.workers)?;
        let (events, events_rx) = mpsc::unbounded_channel();
        Ok(Self {
            runtime: Arc::new(runtime),
            shared: Arc::new(Shared {
                registry,
                stores,
                config,
                known_hosts: OnceCell::new(),
                sessions: Mutex::new(HashMap::new()),
                events,
                next_session: AtomicU64::new(1),
            }),
            events_rx: Arc::new(Mutex::new(Some(events_rx))),
        })
    }

    /// Uses an already loaded `known_hosts` store instead of loading
    /// [`ManagerConfig::known_hosts`] at the first SSH connect.
    ///
    /// # Errors
    /// [`CoreError::Runtime`] when the store was already loaded.
    pub fn set_known_hosts(&self, store: KnownHostsStore) -> Result<(), CoreError> {
        self.shared
            .known_hosts
            .set(store)
            .map_err(|_| CoreError::Runtime("the known_hosts store is already loaded".into()))
    }

    /// The driver registry.
    #[must_use]
    pub fn registry(&self) -> &DriverRegistry {
        &self.shared.registry
    }

    /// The runtime, to spawn other driver-side work (exports, tunnels).
    #[must_use]
    pub fn runtime(&self) -> &DriverRuntime {
        &self.runtime
    }

    /// The session-level event channel. There is one receiver: the first call takes it.
    #[must_use]
    pub fn take_events(&self) -> Option<mpsc::UnboundedReceiver<CoreEvent>> {
        lock(&self.events_rx).take()
    }

    /// Opens a session for a connection, on the runtime. Never blocks.
    ///
    /// The flow: driver lookup and [`Connection::validate_for_driver`]; prompts still needed
    /// ([`OpenOutcome::NeedsSecrets`], before anything touches a store or the network); saved
    /// secrets read on the blocking pool; the SSH tunnel, if any, opened with each hop's host key
    /// policy ([`OpenOutcome::HostKeyNeedsConfirmation`]); the driver's connect, with the policy's
    /// read-only, auto-commit and production flags; a check that a read-only connection got a
    /// read-only session (I3). Secrets are dropped (zeroized) before the driver returns. If the
    /// [`Reply`] is dropped before the answer, a session opened meanwhile is closed.
    pub fn open(&self, request: OpenRequest) -> Reply<Result<OpenOutcome, CoreError>> {
        let (tx, reply) = Reply::channel();
        let shared = Arc::clone(&self.shared);
        self.runtime.spawn(async move {
            let id = request.connection.id;
            let outcome = open_session(&shared, request).await;
            if let Err(e) = &outcome {
                tracing::info!(connection = %id, error = %e, "connection failed");
            }
            if let Err(Ok(OpenOutcome::Opened(handle))) = tx.send(outcome) {
                tracing::debug!(session = %handle.id(), "open request abandoned; closing");
                let _ = handle.close().await;
            }
        });
        reply
    }

    /// Records a host key the user confirmed (from [`OpenOutcome::HostKeyNeedsConfirmation`]) in
    /// NineSQL's `known_hosts`. Call it only after the user saw and accepted the fingerprint.
    pub fn trust_host_key(&self, key: UnknownHostKey) -> Reply<Result<(), CoreError>> {
        let (tx, reply) = Reply::channel();
        let shared = Arc::clone(&self.shared);
        self.runtime.spawn(async move {
            let result = async {
                let store = shared.known_hosts().await?;
                store.trust(&key).await?;
                tracing::info!(
                    hop = key.hop,
                    port = key.port,
                    "SSH host key trusted by the user"
                );
                Ok(())
            }
            .await;
            let _ = tx.send(result);
        });
        reply
    }

    /// The open session with that id.
    #[must_use]
    pub fn session(&self, id: SessionId) -> Option<SessionHandle> {
        lock(&self.shared.sessions).get(&id).cloned()
    }

    /// Open sessions of a connection.
    #[must_use]
    pub fn sessions_of(&self, connection: ConnectionId) -> Vec<SessionHandle> {
        let mut out: Vec<_> = lock(&self.shared.sessions)
            .values()
            .filter(|h| h.info().connection == connection)
            .cloned()
            .collect();
        out.sort_by_key(SessionHandle::id);
        out
    }

    /// Closes every open session (running statements are cancelled first).
    pub fn close_all(&self) -> Reply<()> {
        let (tx, reply) = Reply::channel();
        let handles: Vec<_> = lock(&self.shared.sessions).values().cloned().collect();
        self.runtime.spawn(async move {
            let replies: Vec<_> = handles.iter().map(SessionHandle::close).collect();
            for r in replies {
                let _ = r.await;
            }
            let _ = tx.send(());
        });
        reply
    }

    /// Closes every session, waiting at most `timeout`, then stops the runtime. Blocking: call
    /// it from the main thread at exit, never from an async context.
    pub fn shutdown(self, timeout: Duration) {
        let reply = self.close_all();
        let handle = self.runtime.handle().clone();
        // The timer is created inside the runtime context.
        let _ = handle.block_on(async move { tokio::time::timeout(timeout, reply).await });
    }
}

async fn open_session(
    shared: &Arc<Shared>,
    request: OpenRequest,
) -> Result<OpenOutcome, CoreError> {
    let OpenRequest {
        connection,
        policy,
        prompted,
    } = request;
    if !connection.enabled {
        return Err(CoreError::ConnectionDisabled);
    }
    let driver = shared.registry.driver(&connection.engine)?;
    let engine = api_engine(&connection.engine);
    let pre_caps = driver.capabilities(&engine, None)?;
    connection.validate_for_driver(&pre_caps)?;
    let target = connect::target(&connection)?;

    let supplied: Vec<SecretSlot> = prompted.iter().map(|(s, _)| *s).collect();
    let missing = connect::missing_prompts(&connection, &supplied);
    if !missing.is_empty() {
        return Ok(OpenOutcome::NeedsSecrets(missing));
    }

    let connection = Arc::new(connection);
    let mut bag = {
        let stores = shared.stores.clone();
        let c = Arc::clone(&connection);
        tokio::task::spawn_blocking(move || connect::resolve_secrets(&c, &stores, prompted))
            .await
            .map_err(|_| CoreError::Runtime("the secret resolution task failed".into()))??
    };

    let timeout = shared.config.connect_timeout;
    let mut tunnel = None;
    if let (Some(ssh), connect::Target::Tcp { host, port, .. }) = (&connection.ssh, &target) {
        let spec = connect::tunnel_spec(ssh, &mut bag, host, *port, timeout)?;
        let known_hosts = shared.known_hosts().await?;
        match Tunnel::open(spec, known_hosts).await {
            Ok(t) => tunnel = Some(t),
            Err(SshError::HostKeyNeedsConfirmation(key)) => {
                return Ok(OpenOutcome::HostKeyNeedsConfirmation(key));
            }
            Err(e) => return Err(e.into()),
        }
    }

    let params = connect::connect_params(&connection, &policy, &mut bag, tunnel.as_ref(), timeout)?;
    // Every secret left the bag (into the params or the tunnel); drop it anyway, zeroized.
    drop(bag);
    let mut warnings: Vec<SessionWarning> = params
        .security_warnings()
        .into_iter()
        .map(SessionWarning::Connection)
        .collect();
    if let Some(t) = &tunnel {
        warnings.extend(
            t.security_warnings()
                .iter()
                .cloned()
                .map(SessionWarning::Ssh),
        );
    }
    let tunnel_info = params.tunnel.clone();

    let connected = tokio::time::timeout(timeout + CONNECT_MARGIN, driver.connect(params)).await;
    let session = match connected {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            close_tunnel(tunnel).await;
            return Err(e.into());
        }
        Err(_) => {
            close_tunnel(tunnel).await;
            return Err(CoreError::ConnectTimeout(timeout));
        }
    };
    if policy.read_only && !session.is_read_only() {
        tracing::warn!(connection = %connection.id, "driver did not enforce read-only; session closed");
        let _ = session.close().await;
        close_tunnel(tunnel).await;
        return Err(CoreError::ReadOnlyNotEnforced);
    }
    let capabilities = session.capabilities().clone();
    if session.is_read_only() && capabilities.read_only == ReadOnlyMechanism::ClientSideOnly {
        warnings.push(SessionWarning::ReadOnlyClientSideOnly);
    }

    let id = SessionId(shared.next_session.fetch_add(1, Ordering::SeqCst));
    let info = SessionInfo {
        id,
        connection: connection.id,
        connection_name: connection.name.clone(),
        engine: connection.engine.clone(),
        server_version: session.server_version().map(str::to_owned),
        read_only: session.is_read_only(),
        policy,
        auto_commit: session.auto_commit(),
        tunnel: tunnel_info,
        warnings,
        capabilities,
    };
    let (cmd_tx, cmd_rx) = mpsc::channel(shared.config.command_queue.max(1));
    let (close_tx, close_rx) = oneshot::channel();
    let handle = SessionHandle {
        inner: Arc::new(HandleInner {
            info,
            commands: cmd_tx,
            close: Mutex::new(Some(close_tx)),
            current: Mutex::new(None),
            result_queue: shared.config.result_queue.max(1),
        }),
    };
    lock(&shared.sessions).insert(id, handle.clone());

    let task = SessionTask {
        session: Some(session),
        tunnel,
        handle: handle.clone(),
        confirmations: Confirmations::default(),
        busy_wait: shared.config.busy_wait,
    };
    let join = tokio::spawn(task.run(cmd_rx, close_rx));
    let supervisor = Arc::clone(shared);
    let connection_id = connection.id;
    tokio::spawn(async move {
        let reason = join.await.unwrap_or_else(|e| {
            tracing::error!(session = %id, panicked = e.is_panic(), "session task failed");
            CloseReason::Panicked
        });
        lock(&supervisor.sessions).remove(&id);
        tracing::info!(session = %id, reason = ?reason, "session closed");
        supervisor.emit(CoreEvent::SessionClosed {
            session: id,
            connection: connection_id,
            reason,
        });
    });
    tracing::info!(session = %id, connection = %connection.id, "session opened");
    shared.emit(CoreEvent::SessionOpened {
        session: id,
        connection: connection.id,
    });
    Ok(OpenOutcome::Opened(handle))
}

async fn close_tunnel(tunnel: Option<Tunnel>) {
    if let Some(t) = tunnel {
        t.close().await;
    }
}

// ---------------------------------------------------------------------------------------------
// Session handle and executions

/// Single-use permission to run one statement that needs confirmation, on one session
/// ([`ExecEvent::NeedsConfirmation`]). Bound to the exact statement text; expires after ten
/// minutes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ConfirmationToken(Uuid);

/// Why a statement needs confirmation (ADR-0007 §1, §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ConfirmReason {
    /// Destructive, access-mode changing or unrecognised statement on a type that confirms
    /// destructive statements (always the case for production).
    Destructive,
    /// A write on a type that confirms every write.
    Write,
}

/// A statement waiting for the user's confirmation. The UI shows the exact statement it sent,
/// the connection name and, for production, the production color; then sends the statement
/// again with [`ExecuteRequest::confirmed`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ConfirmationRequest {
    /// Token to send back with the same statement.
    pub token: ConfirmationToken,
    /// Classification of the statement by the session's dialect.
    pub class: StatementClass,
    /// Why it needs confirmation.
    pub reason: ConfirmReason,
    /// Connection name.
    pub connection_name: String,
    /// The connection is typed production.
    pub production: bool,
}

/// One statement to run. `Debug` shows the text length only.
#[derive(Clone)]
#[non_exhaustive]
pub struct ExecuteRequest {
    /// Statement text (one statement).
    pub text: String,
    /// Language.
    pub language: QueryLanguage,
    /// Bind parameters.
    pub params: Vec<Param>,
    /// Wall-clock limit, fetch included; `None` = none (interactive default, ADR-0004).
    pub timeout: Option<Duration>,
    /// Memory budget of the result.
    pub budget: FetchBudget,
    /// Confirmation of a statement that needs one.
    pub confirmation: Option<ConfirmationToken>,
}

impl fmt::Debug for ExecuteRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExecuteRequest")
            .field("text_len", &self.text.len())
            .field("language", &self.language)
            .field("params", &self.params.len())
            .field("timeout", &self.timeout)
            .field("budget", &self.budget)
            .field("confirmed", &self.confirmation.is_some())
            .finish()
    }
}

impl ExecuteRequest {
    /// A SQL statement with the default budget and no timeout.
    #[must_use]
    pub fn sql(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            language: QueryLanguage::Sql,
            params: Vec::new(),
            timeout: None,
            budget: FetchBudget::default(),
            confirmation: None,
        }
    }

    /// Sets the timeout.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.timeout = timeout;
        self
    }

    /// Sets the budget.
    #[must_use]
    pub fn with_budget(mut self, budget: FetchBudget) -> Self {
        self.budget = budget;
        self
    }

    /// Sets bind parameters.
    #[must_use]
    pub fn with_params(mut self, params: Vec<Param>) -> Self {
        self.params = params;
        self
    }

    /// Sets the language.
    #[must_use]
    pub fn with_language(mut self, language: QueryLanguage) -> Self {
        self.language = language;
        self
    }

    /// Attaches the user's confirmation.
    #[must_use]
    pub fn confirmed(mut self, token: ConfirmationToken) -> Self {
        self.confirmation = Some(token);
        self
    }
}

/// Event of one execution. Exactly one terminal event ends the stream:
/// [`ExecEvent::NeedsConfirmation`], [`ExecEvent::Finished`] or [`ExecEvent::Failed`].
#[derive(Debug)]
#[non_exhaustive]
pub enum ExecEvent {
    /// The previous statement was cancelled and the server has not confirmed its end yet; this
    /// statement waits (at most [`ManagerConfig::busy_wait`]).
    WaitingForPreviousCancel,
    /// Not sent: the statement needs the user's confirmation (terminal).
    NeedsConfirmation(ConfirmationRequest),
    /// The statement was sent.
    Started {
        /// Execution id in the session, when the driver records it.
        execution: Option<ExecutionId>,
    },
    /// One result item (header, rows, documents, update count, notice, truncation).
    Item(ResultItem),
    /// The result ended (terminal).
    Finished {
        /// Rows / documents delivered.
        rows: u64,
        /// Estimated bytes delivered.
        bytes: u64,
    },
    /// The statement failed, timed out or was cancelled (terminal):
    /// `CoreError::Driver(DriverError::Cancelled | DriverError::Timeout(_))` for the last two.
    Failed(CoreError),
}

/// A running statement: its events and its cancel path. Dropping it cancels the statement.
pub struct Execution {
    events: mpsc::Receiver<ExecEvent>,
    cancel: CancelToken,
}

impl fmt::Debug for Execution {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Execution")
            .field("cancelled", &self.cancel.is_cancelled())
            .finish_non_exhaustive()
    }
}

impl Execution {
    fn failed(error: CoreError) -> Self {
        let (tx, events) = mpsc::channel(1);
        let _ = tx.try_send(ExecEvent::Failed(error));
        Self {
            events,
            cancel: CancelToken::new(),
        }
    }

    /// Stops the statement: the server-side cancel is sent when the engine has one, and the
    /// execution ends with `Failed(Driver(Cancelled))`. Never blocks; idempotent.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// The cancel token, to cancel from elsewhere.
    #[must_use]
    pub fn cancel_token(&self) -> CancelToken {
        self.cancel.clone()
    }

    /// Next event; `None` after the terminal event.
    pub async fn next(&mut self) -> Option<ExecEvent> {
        self.events.recv().await
    }

    /// Next event without waiting.
    pub fn try_next(&mut self) -> Next<ExecEvent> {
        match self.events.try_recv() {
            Ok(e) => Next::Ready(e),
            Err(mpsc::error::TryRecvError::Empty) => Next::Pending,
            Err(mpsc::error::TryRecvError::Disconnected) => Next::Ended,
        }
    }
}

impl Drop for Execution {
    fn drop(&mut self) {
        // Harmless once the statement ended; stops it otherwise.
        self.cancel.cancel();
    }
}

enum Command {
    Execute {
        request: ExecuteRequest,
        events: mpsc::Sender<ExecEvent>,
        cancel: CancelToken,
    },
    Transaction {
        op: TxOp,
        reply: oneshot::Sender<Result<TransactionState, CoreError>>,
    },
}

#[derive(Debug, Clone, Copy)]
enum TxOp {
    Begin,
    Commit,
    Rollback,
    SetAutoCommit(bool),
    Ping,
}

type CloseSender = oneshot::Sender<oneshot::Sender<Result<(), CoreError>>>;

struct HandleInner {
    info: SessionInfo,
    commands: mpsc::Sender<Command>,
    close: Mutex<Option<CloseSender>>,
    current: Mutex<Option<CancelToken>>,
    result_queue: usize,
}

/// A handle to an open session. Cheap to clone; every method returns at once.
#[derive(Clone)]
pub struct SessionHandle {
    inner: Arc<HandleInner>,
}

impl fmt::Debug for SessionHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionHandle")
            .field("id", &self.inner.info.id)
            .field("connection", &self.inner.info.connection)
            .finish_non_exhaustive()
    }
}

impl SessionHandle {
    /// Session id.
    #[must_use]
    pub fn id(&self) -> SessionId {
        self.inner.info.id
    }

    /// What the session is.
    #[must_use]
    pub fn info(&self) -> &SessionInfo {
        &self.inner.info
    }

    /// `true` once the session task ended.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.inner.commands.is_closed()
    }

    /// Runs one statement. Statements of a session run one after the other, in order; the
    /// session queues a few. Never blocks: a full queue or a closed session yields an execution
    /// that fails at once.
    #[must_use]
    pub fn execute(&self, request: ExecuteRequest) -> Execution {
        let (events_tx, events) = mpsc::channel(self.inner.result_queue);
        let cancel = CancelToken::new();
        let command = Command::Execute {
            request,
            events: events_tx,
            cancel: cancel.clone(),
        };
        match self.inner.commands.try_send(command) {
            Ok(()) => Execution { events, cancel },
            Err(mpsc::error::TrySendError::Full(_)) => {
                Execution::failed(CoreError::Driver(DriverError::Busy))
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                Execution::failed(CoreError::SessionClosed)
            }
        }
    }

    /// Cancels the statement running now, if any (the "Stop" button of the session).
    pub fn cancel_current(&self) {
        if let Some(token) = lock(&self.inner.current).as_ref() {
            token.cancel();
        }
    }

    fn transaction(&self, op: TxOp) -> Reply<Result<TransactionState, CoreError>> {
        let (tx, reply) = Reply::channel();
        match self
            .inner
            .commands
            .try_send(Command::Transaction { op, reply: tx })
        {
            Ok(()) => reply,
            Err(mpsc::error::TrySendError::Full(_)) => {
                Reply::ready(Err(CoreError::Driver(DriverError::Busy)))
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                Reply::ready(Err(CoreError::SessionClosed))
            }
        }
    }

    /// Starts a transaction.
    pub fn begin(&self) -> Reply<Result<TransactionState, CoreError>> {
        self.transaction(TxOp::Begin)
    }

    /// Commits.
    pub fn commit(&self) -> Reply<Result<TransactionState, CoreError>> {
        self.transaction(TxOp::Commit)
    }

    /// Rolls back.
    pub fn rollback(&self) -> Reply<Result<TransactionState, CoreError>> {
        self.transaction(TxOp::Rollback)
    }

    /// Switches auto-commit.
    pub fn set_auto_commit(&self, auto_commit: bool) -> Reply<Result<TransactionState, CoreError>> {
        self.transaction(TxOp::SetAutoCommit(auto_commit))
    }

    /// Liveness check.
    pub fn ping(&self) -> Reply<Result<TransactionState, CoreError>> {
        self.transaction(TxOp::Ping)
    }

    /// Closes the session: cancels the running statement, closes the driver session (rolling
    /// back an open transaction) and then the SSH tunnel. Idempotent.
    pub fn close(&self) -> Reply<Result<(), CoreError>> {
        self.cancel_current();
        let Some(close) = lock(&self.inner.close).take() else {
            return Reply::ready(Ok(()));
        };
        let (tx, reply) = Reply::channel();
        if close.send(tx).is_err() {
            return Reply::ready(Ok(()));
        }
        reply
    }
}

// ---------------------------------------------------------------------------------------------
// Session task

#[derive(Default)]
struct Confirmations {
    pending: HashMap<Uuid, (String, Instant)>,
}

impl Confirmations {
    fn issue(&mut self, text: &str) -> ConfirmationToken {
        let now = Instant::now();
        self.pending
            .retain(|_, (_, at)| now.duration_since(*at) < CONFIRMATION_TTL);
        if self.pending.len() >= MAX_PENDING_CONFIRMATIONS
            && let Some(oldest) = self
                .pending
                .iter()
                .min_by_key(|(_, (_, at))| *at)
                .map(|(k, _)| *k)
        {
            self.pending.remove(&oldest);
        }
        let id = Uuid::new_v4();
        self.pending.insert(id, (text.to_owned(), now));
        ConfirmationToken(id)
    }

    /// Consumes a token: valid when it was issued by this session for exactly `text` and has not
    /// expired. A token is removed even when the text does not match.
    fn redeem(&mut self, token: ConfirmationToken, text: &str) -> bool {
        match self.pending.remove(&token.0) {
            Some((issued_for, at)) => issued_for == text && at.elapsed() < CONFIRMATION_TTL,
            None => false,
        }
    }
}

struct SessionTask {
    session: Option<Box<dyn Session>>,
    tunnel: Option<Tunnel>,
    handle: SessionHandle,
    confirmations: Confirmations,
    busy_wait: Duration,
}

/// What the session loop does after a command.
enum Flow {
    Continue,
    Lost(DriverError),
}

async fn tunnel_lost(tunnel: Option<&Tunnel>) {
    match tunnel {
        Some(t) => {
            t.wait_closed().await;
        }
        None => std::future::pending::<()>().await,
    }
}

impl SessionTask {
    fn id(&self) -> SessionId {
        self.handle.id()
    }

    fn policy(&self) -> &SafetyPolicy {
        &self.handle.info().policy
    }

    async fn run(
        mut self,
        mut commands: mpsc::Receiver<Command>,
        mut close: oneshot::Receiver<oneshot::Sender<Result<(), CoreError>>>,
    ) -> CloseReason {
        let reason = loop {
            tokio::select! {
                biased;
                request = &mut close => {
                    let result = self.shutdown().await;
                    if let Ok(reply) = request {
                        let _ = reply.send(result);
                    }
                    break CloseReason::Requested;
                }
                () = tunnel_lost(self.tunnel.as_ref()) => {
                    tracing::warn!(session = %self.id(), "SSH tunnel lost");
                    let _ = self.shutdown().await;
                    break CloseReason::TunnelLost;
                }
                command = commands.recv() => {
                    let Some(command) = command else {
                        let _ = self.shutdown().await;
                        break CloseReason::Requested;
                    };
                    match self.handle_command(command).await {
                        Flow::Continue => {}
                        Flow::Lost(e) => {
                            let _ = self.shutdown().await;
                            break CloseReason::Lost(e);
                        }
                    }
                }
            }
        };
        // Pending commands fail with `SessionClosed` once the receiver is dropped.
        commands.close();
        while let Ok(command) = commands.try_recv() {
            match command {
                Command::Execute { events, .. } => {
                    let _ = events.try_send(ExecEvent::Failed(CoreError::SessionClosed));
                }
                Command::Transaction { reply, .. } => {
                    let _ = reply.send(Err(CoreError::SessionClosed));
                }
            }
        }
        reason
    }

    async fn shutdown(&mut self) -> Result<(), CoreError> {
        let result = match self.session.take() {
            Some(s) => s.close().await.map_err(CoreError::from),
            None => Ok(()),
        };
        close_tunnel(self.tunnel.take()).await;
        result
    }

    async fn handle_command(&mut self, command: Command) -> Flow {
        match command {
            Command::Execute {
                request,
                events,
                cancel,
            } => {
                *lock(&self.handle.inner.current) = Some(cancel.clone());
                let flow = self.execute(request, &events, &cancel).await;
                *lock(&self.handle.inner.current) = None;
                flow
            }
            Command::Transaction { op, reply } => {
                let Some(session) = self.session.as_mut() else {
                    let _ = reply.send(Err(CoreError::SessionClosed));
                    return Flow::Continue;
                };
                let result = match op {
                    TxOp::Begin => session.begin().await,
                    TxOp::Commit => session.commit().await,
                    TxOp::Rollback => session.rollback().await,
                    TxOp::SetAutoCommit(on) => session.set_auto_commit(on).await,
                    TxOp::Ping => session.ping().await,
                };
                let flow = match &result {
                    Err(e) if e.is_fatal() => Flow::Lost(e.clone()),
                    _ => Flow::Continue,
                };
                let state = TransactionState {
                    auto_commit: session.auto_commit(),
                    in_transaction: session.in_transaction(),
                };
                let _ = reply.send(result.map(|()| state).map_err(CoreError::from));
                flow
            }
        }
    }

    /// Whether `class` needs confirmation under the session's policy.
    fn confirmation_reason(&self, class: StatementClass) -> Option<ConfirmReason> {
        let policy = self.policy();
        if policy.confirm_destructive && class.needs_confirmation() {
            Some(ConfirmReason::Destructive)
        } else if policy.confirm_every_write && class.is_write() {
            Some(ConfirmReason::Write)
        } else {
            None
        }
    }

    async fn execute(
        &mut self,
        request: ExecuteRequest,
        events: &mpsc::Sender<ExecEvent>,
        cancel: &CancelToken,
    ) -> Flow {
        let id = self.id();
        // Cancelled (or its execution dropped) while it waited in the queue: never sent.
        if cancel.is_cancelled() || events.is_closed() {
            let _ = events
                .send(ExecEvent::Failed(CoreError::Driver(DriverError::Cancelled)))
                .await;
            return Flow::Continue;
        }
        let Some(session) = self.session.as_ref() else {
            let _ = events
                .send(ExecEvent::Failed(CoreError::SessionClosed))
                .await;
            return Flow::Continue;
        };
        let class = session.dialect().classify(&request.text);
        if let Some(reason) = self.confirmation_reason(class) {
            let confirmed = request
                .confirmation
                .is_some_and(|t| self.confirmations.redeem(t, &request.text));
            if !confirmed {
                if request.confirmation.is_some() {
                    let _ = events
                        .send(ExecEvent::Failed(CoreError::InvalidConfirmation))
                        .await;
                    return Flow::Continue;
                }
                let token = self.confirmations.issue(&request.text);
                tracing::debug!(session = %id, class = ?class, "statement needs confirmation");
                let info = self.handle.info();
                let _ = events
                    .send(ExecEvent::NeedsConfirmation(ConfirmationRequest {
                        token,
                        class,
                        reason,
                        connection_name: info.connection_name.clone(),
                        production: info.policy.production,
                    }))
                    .await;
                return Flow::Continue;
            }
            tracing::info!(session = %id, class = ?class, "confirmed statement");
        }
        if self.policy().log_query_text {
            tracing::trace!(session = %id, statement = %request.text, "executing");
        } else {
            tracing::debug!(session = %id, class = ?class, "executing");
        }

        let ExecuteRequest {
            text,
            language,
            params,
            timeout,
            budget,
            ..
        } = request;
        let statement = Statement::new(text, language).with_params(params);
        let options = ExecOptions::default()
            .with_timeout(timeout)
            .with_budget(budget)
            .with_cancel(cancel.clone());

        // `Busy` after a cancel: wait for the driver to confirm the end, bounded.
        let started = Instant::now();
        let mut waiting = false;
        let mut stream = loop {
            let Some(session) = self.session.as_mut() else {
                let _ = events
                    .send(ExecEvent::Failed(CoreError::SessionClosed))
                    .await;
                return Flow::Continue;
            };
            match session.execute(statement.clone(), options.clone()).await {
                Ok(stream) => break stream,
                Err(DriverError::Busy) => {
                    if started.elapsed() >= self.busy_wait {
                        let _ = events
                            .send(ExecEvent::Failed(CoreError::StillCancelling))
                            .await;
                        return Flow::Continue;
                    }
                    if !waiting {
                        waiting = true;
                        tracing::debug!(session = %id, "waiting for the previous cancel to be confirmed");
                        let _ = events.send(ExecEvent::WaitingForPreviousCancel).await;
                    }
                    tokio::select! {
                        () = tokio::time::sleep(BUSY_RETRY) => {}
                        () = cancel.cancelled() => {
                            let _ = events
                                .send(ExecEvent::Failed(CoreError::Driver(DriverError::Cancelled)))
                                .await;
                            return Flow::Continue;
                        }
                    }
                }
                Err(e) => {
                    let flow = if e.is_fatal() {
                        Flow::Lost(e.clone())
                    } else {
                        Flow::Continue
                    };
                    let _ = events.send(ExecEvent::Failed(e.into())).await;
                    return flow;
                }
            }
        };

        if events
            .send(ExecEvent::Started {
                execution: stream.execution_id(),
            })
            .await
            .is_err()
        {
            // The consumer is gone: stop the statement (server-side cancel, then drain).
            cancel.cancel();
            let _ = stream.next().await;
            return Flow::Continue;
        }
        loop {
            match stream.next().await {
                None => {
                    let _ = events
                        .send(ExecEvent::Finished {
                            rows: stream.rows_delivered(),
                            bytes: stream.bytes_delivered(),
                        })
                        .await;
                    return Flow::Continue;
                }
                Some(Ok(item)) => {
                    tokio::select! {
                        sent = events.send(ExecEvent::Item(item)) => {
                            if sent.is_err() {
                                cancel.cancel();
                                let _ = stream.next().await;
                                return Flow::Continue;
                            }
                        }
                        // The consumer stopped reading and cancelled: the item is dropped and
                        // the next poll of the stream runs the cancel path.
                        () = cancel.cancelled() => {}
                    }
                }
                Some(Err(e)) => {
                    let flow = if e.is_fatal() {
                        Flow::Lost(e.clone())
                    } else {
                        Flow::Continue
                    };
                    let _ = events.send(ExecEvent::Failed(e.into())).await;
                    return flow;
                }
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).
mod tests {
    use super::*;

    #[test]
    fn confirmation_tokens_are_single_use_and_bound_to_the_text() {
        let mut c = Confirmations::default();
        let t = c.issue("DROP TABLE a");
        assert!(!c.redeem(t, "DROP TABLE b"));
        assert!(
            !c.redeem(t, "DROP TABLE a"),
            "consumed by the failed attempt"
        );
        let t = c.issue("DROP TABLE a");
        assert!(c.redeem(t, "DROP TABLE a"));
        assert!(!c.redeem(t, "DROP TABLE a"));
        for i in 0..(MAX_PENDING_CONFIRMATIONS * 2) {
            c.issue(&format!("DROP TABLE t{i}"));
        }
        assert!(c.pending.len() <= MAX_PENDING_CONFIRMATIONS);
    }

    #[tokio::test]
    async fn reply_can_be_polled_or_awaited() {
        let (tx, mut reply) = Reply::<u8>::channel();
        assert!(matches!(reply.try_take(), Next::Pending));
        tx.send(7).unwrap();
        assert!(matches!(reply.try_take(), Next::Ready(7)));
        assert!(matches!(reply.try_take(), Next::Ended));
        assert_eq!(Reply::ready(3u8).await, Some(3));
        let (tx, reply) = Reply::<u8>::channel();
        drop(tx);
        assert_eq!(reply.await, None);
    }
}
