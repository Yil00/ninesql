//! Metadata service (P4-B core, F-NAV-03 / F-NAV-04 / F-NAV-05, editor object completion):
//! lazy, cancellable, single-flight loading of navigator nodes through the driver runtime, a
//! bounded cache per connection, DDL, navigator filters, quick search and completion snapshots.
//!
//! # Model
//! - A **node** ([`NodeKey`]) is "the objects of one kind under one parent", the unit the driver
//!   lists ([`Metadata::list`]). [`MetadataService::child_kinds`] says which kinds an object has.
//! - Requests name a **connection**: the service binds it to the connection's oldest open
//!   session and keeps one cache per connection, bound to that session. When the session ends,
//!   the cache is cleared, requests in flight fail with [`MetadataError::SessionClosed`], and
//!   [`MetadataEvent::Invalidated`] tells the UI; the next request binds another open session of
//!   the connection, if any.
//! - While a transfer (export or CSV import, `crate::transfer`) leases the bound session, every
//!   request fails at once with [`MetadataError::SessionLeased`], and a load already running
//!   stops before its next driver call: no metadata statement runs inside the transfer's
//!   transaction. A driver call that had already started when the lease was taken finishes
//!   first (the session serialises it before the transfer's first statement). The UI loads
//!   again once the transfer ended.
//! - Every call runs on the driver runtime (I6) and answers through a [`MetadataRequest`]
//!   (await it on any executor, or poll it with [`MetadataRequest::try_take`]) and an event on
//!   the [`MetadataEvents`] channel.
//! - **One load at a time per session.** A driver session runs one execution at a time and
//!   answers a second one with [`DriverError::Busy`] (ADR-0004), and metadata calls share the
//!   session's connection. Loads of the service on one session therefore wait in a FIFO queue
//!   (the session's metadata gate, shared by every `MetadataService` bound to that session)
//!   and run one after the other: the service's own loads never fail with `Busy` because of
//!   each other. Waiting happens on the driver runtime, never on the caller's thread (I6).
//! - **Statements first.** While a statement or transaction command is queued or running on
//!   the session (a paged result paused for the grid included, which holds the session until
//!   it is stopped or read to its end), a load fails quickly, before any driver call, with
//!   `MetadataError::Driver(DriverError::Busy)` (kind `driver-busy`): it waits at most
//!   250 ms (a statement that just sent its last event is recorded as ended a moment later),
//!   never behind a result that may stay open for minutes. A `Busy` answered by the driver
//!   (an abandoned metadata call still confirming its server-side cancel, or a statement that
//!   started meanwhile) is retried every 25 ms for up to the driver's cancel grace plus 1 s,
//!   within the load's timeout.
//!
//! # Guarantees
//! - **Capabilities first** (I8): a kind the session does not declare in
//!   [`MetadataCapabilities::kinds`] (or that the driver does not list under that parent), and
//!   DDL for a kind not in [`MetadataCapabilities::ddl`], fail with
//!   [`MetadataError::Unsupported`] / [`MetadataError::DdlUnsupported`] before any driver call.
//! - **Single flight**: one request in flight per node (and per DDL object); later callers
//!   share it.
//! - **Cancel on drop**: dropping a [`MetadataRequest`] withdraws its interest; when no one waits
//!   any more, the driver call is abandoned (its future dropped, as the trait allows).
//! - **Timeout**: each load gets [`MetadataConfig::request_timeout`], counted from the moment it
//!   was queued: the time spent waiting behind other loads of the session counts, so every
//!   request answers within the timeout.
//! - **Bounded**: a node holds at most `page_size × max_pages_per_load` objects per load
//!   ([`MetadataService::load_more`] continues); the cache of a connection holds at most
//!   [`MetadataConfig::max_nodes_per_connection`] entries and
//!   [`MetadataConfig::max_bytes_per_connection`] estimated bytes, least recently used evicted
//!   first; at most one driver call runs per session (see above).
//! - **No secret**: metadata holds object names, comments, types and DDL, never a credential;
//!   logs carry connection ids, kind names and counts only, never object names or engine
//!   messages (ADR-0007 §6).
//! - **Search never crawls**: [`MetadataService::search`] reads the cache only. A background
//!   [`MetadataService::prefetch`] (or [`MetadataService::warm_schema`]) is explicit, bounded
//!   and cancelled when its handle is dropped; nothing prefetches by default.

mod cache;
pub mod completion;
pub mod filters;
pub mod model;
pub mod search;

use std::collections::hash_map::Entry;
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use ninesql_driver_api::metadata::{Metadata, ObjectInfo, ObjectPath, PageRequest};
use ninesql_driver_api::{DriverError, MetadataCapabilities, ObjectKind};
use tokio::sync::{Semaphore, mpsc, oneshot};

/// Wait between two attempts when the driver answers `Busy` while no statement is pending (an
/// abandoned metadata call still confirming its cancel).
const BUSY_RETRY: Duration = Duration::from_millis(25);

/// How long a load waits for a statement of its session to be recorded as ended before it
/// fails with `Busy`: covers a statement that just sent its last event (the session task
/// records its end right after), not a running or paused result.
const STATEMENT_GRACE: Duration = Duration::from_millis(250);
const STATEMENT_POLL: Duration = Duration::from_millis(10);
use tokio::task::AbortHandle;

use self::cache::{CacheKey, CacheValue, LruCache};
pub use self::completion::{CompletionColumn, CompletionObject, CompletionSnapshot, ForeignKeyRef};
pub use self::filters::{
    ConnectionFilters, FilterError, FilterIssue, KindFilter, NamePattern, NavigatorFile,
    NavigatorFilters,
};
pub use self::search::{MatchField, SearchHit, SearchRequest, SearchResults, fuzzy_score};
use crate::connection::ConnectionId;
use crate::session::{Next, Reply, SessionHandle, SessionId, SessionManager};
use crate::workspace::Workspace;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

// ---------------------------------------------------------------------------------------------
// Configuration

/// Limits of the metadata service. [`Default`] gives the values below.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct MetadataConfig {
    /// Time allowed to one load (every page of it) or one DDL fetch, counted from the moment
    /// it was queued behind the session's other loads. Default 30 s.
    pub request_timeout: Duration,
    /// Objects per driver page (1..=10 000). Default [`PageRequest::DEFAULT_LIMIT`].
    pub page_size: u32,
    /// Pages fetched by one load before it stops and reports the node incomplete (1..=100).
    /// Default 10.
    pub max_pages_per_load: u32,
    /// Most cache entries (nodes and DDL texts) per connection. Default 4 096.
    pub max_nodes_per_connection: usize,
    /// Estimated memory budget of one connection's cache, in bytes. Default 64 MiB.
    pub max_bytes_per_connection: usize,
    /// Kept for compatibility; no longer used. Driver sessions run one execution at a time, so
    /// the service runs at most one load at a time per session and queues the others
    /// (the module documentation). Default 4.
    pub max_concurrent_requests: usize,
    /// Capacity of the event channel. Default 1 024.
    pub event_queue: usize,
}

impl Default for MetadataConfig {
    fn default() -> Self {
        Self {
            request_timeout: Duration::from_secs(30),
            page_size: PageRequest::DEFAULT_LIMIT,
            max_pages_per_load: 10,
            max_nodes_per_connection: 4_096,
            max_bytes_per_connection: 64 * 1024 * 1024,
            max_concurrent_requests: 4,
            event_queue: 1_024,
        }
    }
}

impl MetadataConfig {
    fn clamped(mut self) -> Self {
        self.page_size = self.page_size.clamp(1, 10_000);
        self.max_pages_per_load = self.max_pages_per_load.clamp(1, 100);
        self.max_nodes_per_connection = self.max_nodes_per_connection.max(1);
        self.max_bytes_per_connection = self.max_bytes_per_connection.max(1);
        self.max_concurrent_requests = self.max_concurrent_requests.clamp(1, 64);
        self.event_queue = self.event_queue.max(1);
        if self.request_timeout.is_zero() {
            self.request_timeout = Duration::from_secs(30);
        }
        self
    }
}

// ---------------------------------------------------------------------------------------------
// Public data

/// A navigator node: the objects of `kind` directly under `parent`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeKey {
    /// The parent object (the root for top-level objects).
    pub parent: ObjectPath,
    /// The kind listed.
    pub kind: ObjectKind,
}

impl NodeKey {
    /// The node of `kind` under `parent`.
    #[must_use]
    pub fn new(parent: ObjectPath, kind: ObjectKind) -> Self {
        Self { parent, kind }
    }
}

/// The loaded children of a node. Shared (`Arc`) between the cache, the UI and search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeChildren {
    /// The node.
    pub key: NodeKey,
    /// The objects shown, in the driver's order, after filters.
    pub items: Vec<ObjectInfo>,
    /// `false` when the server has more ([`MetadataService::load_more`]).
    pub complete: bool,
    /// Objects the navigator filters hid (client-side; a server-side filter is not counted).
    pub hidden_by_filter: usize,
    /// When it was loaded.
    pub loaded_at: Instant,
    pub(crate) next_cursor: Option<String>,
    pub(crate) bytes: usize,
}

/// The DDL of an object, as the driver produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectDdl {
    /// The object.
    pub path: ObjectPath,
    /// The DDL text.
    pub text: String,
    /// When it was loaded.
    pub loaded_at: Instant,
    pub(crate) bytes: usize,
}

/// Why a metadata request failed. User-presentable; never carries a secret. Engine messages
/// (in [`MetadataError::Driver`]) may name objects and are shown to the user, never logged.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum MetadataError {
    /// The connection has no open session.
    #[error("the connection is not open")]
    NoSession,
    /// The session is held by a transfer (export or CSV import, whose transaction may be
    /// open): no metadata call runs on it until the transfer ends; load again then.
    #[error("an export or import is using this connection's session; try again when it ends")]
    SessionLeased,
    /// The session closed before the answer.
    #[error("the session is closed")]
    SessionClosed,
    /// The session does not list this kind of object here (I8).
    #[error("this kind of object is not available for this connection")]
    Unsupported(ObjectKind),
    /// The session cannot produce DDL for this kind of object (I8).
    #[error("DDL is not available for this kind of object")]
    DdlUnsupported(Option<ObjectKind>),
    /// The request took longer than [`MetadataConfig::request_timeout`].
    #[error("loading metadata timed out after {} s", .0.as_secs())]
    Timeout(Duration),
    /// The driver failed.
    #[error(transparent)]
    Driver(DriverError),
    /// The metadata service stopped before answering.
    #[error("the metadata request was cancelled")]
    Cancelled,
    /// Navigator filters could not be saved.
    #[error("the navigator filters could not be saved: {0}")]
    Save(String),
}

impl MetadataError {
    /// A short, stable name, safe to log at any level.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::NoSession => "no-session",
            Self::SessionLeased => "session-leased",
            Self::SessionClosed => "session-closed",
            Self::Unsupported(_) => "unsupported-kind",
            Self::DdlUnsupported(_) => "ddl-unsupported",
            Self::Timeout(_) => "timeout",
            Self::Driver(e) => crate::error::driver_error_kind(e),
            Self::Cancelled => "cancelled",
            Self::Save(_) => "save",
        }
    }
}

/// What an invalidation covers.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum InvalidationScope {
    /// Everything cached for the connection.
    Connection,
    /// An object and everything under it.
    Subtree(ObjectPath),
    /// A node and everything under its objects ([`MetadataService::refresh`]).
    Node(NodeKey),
}

/// Why cached metadata was dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum InvalidationReason {
    /// [`MetadataService::invalidate`].
    Requested,
    /// [`MetadataService::refresh`].
    Refresh,
    /// The connection's navigator filters changed.
    FiltersChanged,
    /// The session the cache was bound to ended.
    SessionClosed,
}

/// Event for the UI ([`MetadataService::take_events`]).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum MetadataEvent {
    /// A node finished loading (or loaded more).
    NodeLoaded {
        /// The connection.
        connection: ConnectionId,
        /// The node.
        key: NodeKey,
        /// Objects shown.
        items: usize,
        /// `false` when the server has more.
        complete: bool,
        /// Objects hidden by filters.
        hidden_by_filter: usize,
        /// The result is in the cache ([`MetadataService::cached_children`]); `false` when it
        /// was larger than the cache budget or invalidated while loading (the requester still
        /// got it).
        cached: bool,
    },
    /// A node failed to load.
    NodeFailed {
        /// The connection.
        connection: ConnectionId,
        /// The node.
        key: NodeKey,
        /// Why (presentable).
        error: MetadataError,
    },
    /// The DDL of an object was loaded.
    DdlLoaded {
        /// The connection.
        connection: ConnectionId,
        /// The object.
        path: ObjectPath,
        /// In the cache ([`MetadataService::cached_ddl`]).
        cached: bool,
    },
    /// The DDL of an object failed.
    DdlFailed {
        /// The connection.
        connection: ConnectionId,
        /// The object.
        path: ObjectPath,
        /// Why (presentable).
        error: MetadataError,
    },
    /// Cached metadata was dropped: the UI reloads what it shows under `scope`.
    Invalidated {
        /// The connection.
        connection: ConnectionId,
        /// What was dropped.
        scope: InvalidationScope,
        /// Why.
        reason: InvalidationReason,
    },
}

/// Size of one connection's cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct CacheStats {
    /// The session the cache is bound to.
    pub session: SessionId,
    /// Entries (nodes and DDL texts).
    pub entries: usize,
    /// Estimated bytes.
    pub bytes: usize,
    /// Entries evicted so far.
    pub evictions: u64,
    /// Bumped on every change.
    pub generation: u64,
    /// Requests in flight.
    pub in_flight: usize,
}

// ---------------------------------------------------------------------------------------------
// Events channel

/// The receiver of [`MetadataEvent`]s. Bounded: events that do not fit are dropped and counted
/// ([`MetadataService::dropped_events`]); the UI then re-reads what it shows from the cache.
pub struct MetadataEvents {
    rx: mpsc::Receiver<MetadataEvent>,
}

impl fmt::Debug for MetadataEvents {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MetadataEvents").finish_non_exhaustive()
    }
}

impl MetadataEvents {
    /// Next event without waiting.
    pub fn try_recv(&mut self) -> Next<MetadataEvent> {
        match self.rx.try_recv() {
            Ok(e) => Next::Ready(e),
            Err(mpsc::error::TryRecvError::Empty) => Next::Pending,
            Err(mpsc::error::TryRecvError::Disconnected) => Next::Ended,
        }
    }

    /// Next event; `None` once the service is gone.
    pub async fn recv(&mut self) -> Option<MetadataEvent> {
        self.rx.recv().await
    }
}

// ---------------------------------------------------------------------------------------------
// Requests

type FlightResult = Result<CacheValue, MetadataError>;

/// The answer to a metadata request. Await it on any executor or poll it with
/// [`Self::try_take`]. **Dropping it cancels** the request unless someone else waits for the
/// same node.
pub struct MetadataRequest<T> {
    rx: Option<oneshot::Receiver<FlightResult>>,
    extract: fn(CacheValue) -> Option<T>,
    _guard: Option<WaiterGuard>,
}

impl<T> fmt::Debug for MetadataRequest<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MetadataRequest")
            .field("taken", &self.rx.is_none())
            .finish_non_exhaustive()
    }
}

impl<T> MetadataRequest<T> {
    fn ready(value: FlightResult, extract: fn(CacheValue) -> Option<T>) -> Self {
        let (tx, rx) = oneshot::channel();
        let _ = tx.send(value);
        Self {
            rx: Some(rx),
            extract,
            _guard: None,
        }
    }

    fn convert(
        &self,
        r: Result<FlightResult, oneshot::error::RecvError>,
    ) -> Result<T, MetadataError> {
        match r {
            Ok(Ok(v)) => (self.extract)(v).ok_or(MetadataError::Cancelled),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(MetadataError::Cancelled),
        }
    }

    /// Takes the answer if it is there. Never blocks.
    pub fn try_take(&mut self) -> Next<Result<T, MetadataError>> {
        let Some(rx) = self.rx.as_mut() else {
            return Next::Ended;
        };
        let r = match rx.try_recv() {
            Ok(v) => Ok(v),
            Err(oneshot::error::TryRecvError::Empty) => return Next::Pending,
            Err(oneshot::error::TryRecvError::Closed) => Err(()),
        };
        self.rx = None;
        Next::Ready(match r {
            Ok(v) => self.convert(Ok(v)),
            Err(()) => Err(MetadataError::Cancelled),
        })
    }
}

impl<T> Future for MetadataRequest<T> {
    type Output = Result<T, MetadataError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let Some(rx) = self.rx.as_mut() else {
            return Poll::Ready(Err(MetadataError::Cancelled));
        };
        match Pin::new(rx).poll(cx) {
            Poll::Ready(r) => {
                self.rx = None;
                Poll::Ready(self.convert(r))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

fn as_children(v: CacheValue) -> Option<Arc<NodeChildren>> {
    match v {
        CacheValue::Children(c) => Some(c),
        CacheValue::Ddl(_) => None,
    }
}

fn as_ddl(v: CacheValue) -> Option<Arc<ObjectDdl>> {
    match v {
        CacheValue::Ddl(d) => Some(d),
        CacheValue::Children(_) => None,
    }
}

/// Withdraws a waiter when its request is dropped; the last one cancels the flight.
struct WaiterGuard {
    inner: Weak<Inner>,
    connection: ConnectionId,
    key: FlightKey,
    waiter: u64,
}

impl Drop for WaiterGuard {
    fn drop(&mut self) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        let mut st = lock(&inner.state);
        let Some(cs) = st.conns.get_mut(&self.connection) else {
            return;
        };
        let last = match cs.flights.get_mut(&self.key) {
            Some(f) => f.waiters.remove(&self.waiter).is_some() && f.waiters.is_empty(),
            None => false,
        };
        if last && let Some(f) = cs.flights.remove(&self.key) {
            f.abort.abort();
            tracing::debug!(connection = %self.connection, "metadata request abandoned");
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Service state

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum FlightKey {
    Children(NodeKey),
    More(NodeKey),
    Ddl(ObjectPath),
}

impl FlightKey {
    fn cache_key(&self) -> CacheKey {
        match self {
            Self::Children(k) | Self::More(k) => CacheKey::Children(k.clone()),
            Self::Ddl(p) => CacheKey::Ddl(p.clone()),
        }
    }
}

struct Flight {
    id: u64,
    waiters: HashMap<u64, oneshot::Sender<FlightResult>>,
    abort: AbortHandle,
    /// Invalidated while in flight: delivered to its waiters, never cached, and not joined by
    /// later requests.
    stale: bool,
}

struct ConnState {
    session: SessionHandle,
    metadata: Arc<dyn Metadata>,
    caps: MetadataCapabilities,
    cache: LruCache,
    flights: HashMap<FlightKey, Flight>,
    watcher: AbortHandle,
    snapshot: Option<Arc<CompletionSnapshot>>,
}

impl ConnState {
    /// Ends every flight: waiters get `error`, tasks are aborted.
    fn fail_flights(&mut self, error: &MetadataError) {
        for (_, f) in self.flights.drain() {
            f.abort.abort();
            for (_, tx) in f.waiters {
                let _ = tx.send(Err(error.clone()));
            }
        }
    }

    fn mark_stale(&mut self, pred: impl Fn(&FlightKey) -> bool) {
        for (k, f) in &mut self.flights {
            if pred(k) {
                f.stale = true;
            }
        }
    }
}

impl Drop for ConnState {
    fn drop(&mut self) {
        self.watcher.abort();
        self.fail_flights(&MetadataError::Cancelled);
    }
}

#[derive(Default)]
struct State {
    conns: HashMap<ConnectionId, ConnState>,
    filters: NavigatorFilters,
}

struct Inner {
    manager: SessionManager,
    config: MetadataConfig,
    state: Mutex<State>,
    events: mpsc::Sender<MetadataEvent>,
    events_rx: Mutex<Option<MetadataEvents>>,
    dropped: AtomicU64,
    next_id: AtomicU64,
    workspace: Option<Workspace>,
}

/// One writer of `navigator.json` at a time in the process.
static NAVIGATOR_WRITE: Mutex<()> = Mutex::new(());

impl Inner {
    fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    fn emit(&self, event: MetadataEvent) {
        if let Err(mpsc::error::TrySendError::Full(_)) = self.events.try_send(event) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// The connection's state, bound to an open session.
    fn bind<'a>(
        self: &Arc<Self>,
        st: &'a mut State,
        connection: ConnectionId,
    ) -> Result<&'a mut ConnState, MetadataError> {
        let stale = st
            .conns
            .get(&connection)
            .is_some_and(|cs| cs.session.is_closed());
        if stale {
            self.drop_connection(st, connection, InvalidationReason::SessionClosed);
        }
        // A transfer holds the session (its transaction may be open): nothing runs on it
        // meanwhile; the navigator says so and loads again once the transfer ended.
        if st
            .conns
            .get(&connection)
            .is_some_and(|cs| cs.session.is_leased())
        {
            return Err(MetadataError::SessionLeased);
        }
        let vacant = match st.conns.entry(connection) {
            Entry::Occupied(o) => return Ok(o.into_mut()),
            Entry::Vacant(v) => v,
        };
        {
            let handle = self
                .manager
                .sessions_of(connection)
                .into_iter()
                .find(|h| !h.is_closed())
                .ok_or(MetadataError::NoSession)?;
            if handle.is_leased() {
                return Err(MetadataError::SessionLeased);
            }
            let session = handle.id();
            let weak = Arc::downgrade(self);
            let watched = handle.clone();
            let watcher = self
                .manager
                .runtime()
                .spawn(async move {
                    watched.closed().await;
                    if let Some(inner) = weak.upgrade() {
                        inner.session_closed(connection, session);
                    }
                })
                .abort_handle();
            let caps = handle.info().capabilities.metadata.clone();
            tracing::debug!(connection = %connection, session = %session, "metadata cache bound");
            Ok(vacant.insert(ConnState {
                metadata: handle.metadata(),
                session: handle,
                caps,
                cache: LruCache::new(
                    self.config.max_nodes_per_connection,
                    self.config.max_bytes_per_connection,
                ),
                flights: HashMap::new(),
                watcher,
                snapshot: None,
            }))
        }
    }

    fn drop_connection(
        &self,
        st: &mut State,
        connection: ConnectionId,
        reason: InvalidationReason,
    ) {
        if let Some(mut cs) = st.conns.remove(&connection) {
            cs.fail_flights(&MetadataError::SessionClosed);
            tracing::debug!(connection = %connection, entries = cs.cache.len(), "metadata cache cleared");
            self.emit(MetadataEvent::Invalidated {
                connection,
                scope: InvalidationScope::Connection,
                reason,
            });
        }
    }

    fn session_closed(&self, connection: ConnectionId, session: SessionId) {
        let mut st = lock(&self.state);
        if st
            .conns
            .get(&connection)
            .is_some_and(|cs| cs.session.id() == session)
        {
            self.drop_connection(&mut st, connection, InvalidationReason::SessionClosed);
        }
    }

    fn check_supported(cs: &ConnState, key: &FlightKey) -> Result<(), MetadataError> {
        match key {
            FlightKey::Children(k) | FlightKey::More(k) => {
                let declared = cs.caps.kinds.contains(&k.kind)
                    && cs.metadata.child_kinds(k.parent.kind()).contains(&k.kind);
                if declared {
                    Ok(())
                } else {
                    Err(MetadataError::Unsupported(k.kind))
                }
            }
            FlightKey::Ddl(p) => match p.kind() {
                Some(kind) if cs.caps.ddl.contains(&kind) => Ok(()),
                other => Err(MetadataError::DdlUnsupported(other)),
            },
        }
    }

    fn request<T>(
        self: &Arc<Self>,
        connection: ConnectionId,
        mut key: FlightKey,
        extract: fn(CacheValue) -> Option<T>,
    ) -> MetadataRequest<T> {
        let mut guard = lock(&self.state);
        let st = &mut *guard;
        let filter = match &key {
            FlightKey::Children(k) | FlightKey::More(k) => {
                st.filters.of(connection).get(k.kind).cloned()
            }
            FlightKey::Ddl(_) => None,
        };
        let cs = match self.bind(st, connection) {
            Ok(cs) => cs,
            Err(e) => return MetadataRequest::ready(Err(e), extract),
        };
        if let Err(e) = Self::check_supported(cs, &key) {
            return MetadataRequest::ready(Err(e), extract);
        }
        let cache_key = key.cache_key();
        // `load_more` continues the cached node; without one it is a plain load.
        let base = match &key {
            FlightKey::More(k) => match cs.cache.get(&cache_key) {
                Some(CacheValue::Children(c)) if c.complete => {
                    return MetadataRequest::ready(Ok(CacheValue::Children(c)), extract);
                }
                Some(CacheValue::Children(c)) => Some(c),
                _ => {
                    key = FlightKey::Children(k.clone());
                    None
                }
            },
            _ => None,
        };
        if matches!(key, FlightKey::Children(_) | FlightKey::Ddl(_))
            && let Some(v) = cs.cache.get(&cache_key)
        {
            return MetadataRequest::ready(Ok(v), extract);
        }
        let waiter = self.next_id();
        let (tx, rx) = oneshot::channel();
        let request = MetadataRequest {
            rx: Some(rx),
            extract,
            _guard: Some(WaiterGuard {
                inner: Arc::downgrade(self),
                connection,
                key: key.clone(),
                waiter,
            }),
        };
        // Join a flight in progress, unless it was invalidated meanwhile: then a new flight
        // takes over its waiters, so they get fresh data.
        let mut waiters = HashMap::new();
        if let Some(f) = cs.flights.get_mut(&key)
            && !f.stale
        {
            f.waiters.insert(waiter, tx);
            return request;
        }
        if let Some(old) = cs.flights.remove(&key) {
            old.abort.abort();
            waiters = old.waiters;
        }
        waiters.insert(waiter, tx);
        let id = self.next_id();
        let session = cs.session.id();
        let work = Work {
            session: cs.session.clone(),
            metadata: Arc::clone(&cs.metadata),
            gate: cs.session.metadata_gate(),
            key: key.clone(),
            base,
            filter,
            page_size: self.config.page_size,
            max_pages: self.config.max_pages_per_load,
            timeout: self.config.request_timeout,
        };
        let weak = Arc::downgrade(self);
        let flight_key = key.clone();
        let abort = self
            .manager
            .runtime()
            .spawn(async move {
                let result = work.run().await;
                if let Some(inner) = weak.upgrade() {
                    inner.finish(connection, session, &flight_key, id, result);
                }
            })
            .abort_handle();
        cs.flights.insert(
            key,
            Flight {
                id,
                waiters,
                abort,
                stale: false,
            },
        );
        request
    }

    fn finish(
        &self,
        connection: ConnectionId,
        session: SessionId,
        key: &FlightKey,
        id: u64,
        result: FlightResult,
    ) {
        let mut st = lock(&self.state);
        let Some(cs) = st.conns.get_mut(&connection) else {
            return;
        };
        if cs.session.id() != session || cs.flights.get(key).is_none_or(|f| f.id != id) {
            return;
        }
        let Some(flight) = cs.flights.remove(key) else {
            return;
        };
        let cached = match &result {
            Ok(v) if !flight.stale => cs.cache.insert(key.cache_key(), v.clone()),
            _ => false,
        };
        let event = match (&result, key) {
            (Ok(CacheValue::Children(c)), FlightKey::Children(k) | FlightKey::More(k)) => {
                tracing::debug!(
                    connection = %connection,
                    kind = model::kind_name(k.kind).unwrap_or("other"),
                    items = c.items.len(),
                    complete = c.complete,
                    cached,
                    "metadata node loaded"
                );
                Some(MetadataEvent::NodeLoaded {
                    connection,
                    key: k.clone(),
                    items: c.items.len(),
                    complete: c.complete,
                    hidden_by_filter: c.hidden_by_filter,
                    cached,
                })
            }
            (Err(e), FlightKey::Children(k) | FlightKey::More(k)) => {
                tracing::info!(
                    connection = %connection,
                    kind = model::kind_name(k.kind).unwrap_or("other"),
                    error = e.kind(),
                    "metadata node failed"
                );
                Some(MetadataEvent::NodeFailed {
                    connection,
                    key: k.clone(),
                    error: e.clone(),
                })
            }
            (Ok(_), FlightKey::Ddl(p)) => Some(MetadataEvent::DdlLoaded {
                connection,
                path: p.clone(),
                cached,
            }),
            (Err(e), FlightKey::Ddl(p)) => {
                tracing::info!(connection = %connection, error = e.kind(), "DDL fetch failed");
                Some(MetadataEvent::DdlFailed {
                    connection,
                    path: p.clone(),
                    error: e.clone(),
                })
            }
            _ => None,
        };
        for (_, tx) in flight.waiters {
            let _ = tx.send(result.clone());
        }
        drop(st);
        if let Some(e) = event {
            self.emit(e);
        }
    }
}

/// One flight's work, run on the driver runtime.
struct Work {
    /// Checked before every driver call: no metadata call starts on a leased session.
    session: crate::session::SessionHandle,
    metadata: Arc<dyn Metadata>,
    /// The session's metadata queue: one load at a time per session.
    gate: Arc<Semaphore>,
    key: FlightKey,
    base: Option<Arc<NodeChildren>>,
    filter: Option<KindFilter>,
    page_size: u32,
    max_pages: u32,
    timeout: Duration,
}

impl Work {
    async fn run(self) -> FlightResult {
        // One load at a time per session (FIFO); the timeout covers the wait in the queue too,
        // so a request answers within it whatever runs before it. The permit is released when
        // the load ends, times out or is abandoned (its future dropped).
        let timeout = self.timeout;
        let load = async {
            // Fail fast behind a statement rather than queue behind it.
            self.wait_for_statements().await?;
            let Ok(_permit) = Arc::clone(&self.gate).acquire_owned().await else {
                return Err(MetadataError::Cancelled);
            };
            self.call().await
        };
        match tokio::time::timeout(timeout, load).await {
            Ok(r) => r,
            Err(_) => Err(MetadataError::Timeout(timeout)),
        }
    }

    /// Checked before every driver call: no metadata call starts on a leased session
    /// ([`MetadataError::SessionLeased`] at once), nor while a statement or transaction command
    /// of the session is queued or running, a paused paged result included. A statement that
    /// just sent its last event is recorded as ended a moment later, so the check waits up to
    /// [`STATEMENT_GRACE`] for that; past it the driver would answer `Busy`, so the service
    /// does, without queueing behind a result that may stay open for minutes.
    async fn wait_for_statements(&self) -> Result<(), MetadataError> {
        let started = Instant::now();
        loop {
            if self.session.is_leased() {
                return Err(MetadataError::SessionLeased);
            }
            if !self.session.has_pending_commands() {
                return Ok(());
            }
            if started.elapsed() >= STATEMENT_GRACE {
                return Err(MetadataError::Driver(DriverError::Busy));
            }
            tokio::time::sleep(STATEMENT_POLL).await;
        }
    }

    /// One driver call, after [`Self::wait_for_statements`]. A `Busy` from the driver is
    /// retried: either a statement started meanwhile (the next check fails after its grace) or
    /// an earlier metadata call of the service was abandoned (timeout, request dropped) and
    /// the driver is still confirming its server-side cancel. Bounded by the driver's cancel
    /// grace plus 1 s (and the load's timeout).
    async fn driver_call<T, F, Fut>(&self, mut call: F) -> Result<T, MetadataError>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T, DriverError>>,
    {
        let grace = ninesql_driver_api::SERVER_CANCEL_GRACE + Duration::from_secs(1);
        let started = Instant::now();
        loop {
            self.wait_for_statements().await?;
            match call().await {
                Err(DriverError::Busy) if started.elapsed() < grace => {
                    tokio::time::sleep(BUSY_RETRY).await;
                }
                other => return other.map_err(MetadataError::Driver),
            }
        }
    }

    async fn call(&self) -> FlightResult {
        match &self.key {
            FlightKey::Children(k) | FlightKey::More(k) => self.list(k).await,
            FlightKey::Ddl(path) => {
                let text = self.driver_call(|| self.metadata.ddl(path)).await?;
                let bytes = text.len() + model::path_bytes(path) + 128;
                Ok(CacheValue::Ddl(Arc::new(ObjectDdl {
                    path: path.clone(),
                    text,
                    loaded_at: Instant::now(),
                    bytes,
                })))
            }
        }
    }

    async fn list(&self, key: &NodeKey) -> FlightResult {
        let server = self.filter.as_ref().and_then(KindFilter::server_substring);
        let (mut items, mut hidden, mut cursor) = match &self.base {
            Some(b) => (b.items.clone(), b.hidden_by_filter, b.next_cursor.clone()),
            None => (Vec::new(), 0, None),
        };
        let depth = key.parent.0.len();
        let mut added = 0usize;
        let mut pages = 0u32;
        loop {
            let request = PageRequest::first()
                .with_limit(self.page_size)
                .with_cursor(cursor.clone())
                .with_filter(server.clone());
            let page = self
                .driver_call(|| self.metadata.list(&key.parent, key.kind, request.clone()))
                .await?;
            pages += 1;
            for item in page.items {
                // A driver that answers with objects from elsewhere is not trusted with the
                // tree: only direct children of the requested kind are kept.
                let direct = item.path.0.len() == depth + 1
                    && item.path.kind() == Some(key.kind)
                    && model::is_under(&item.path, &key.parent);
                if !direct {
                    continue;
                }
                if self.filter.as_ref().is_none_or(|f| f.accepts(item.name())) {
                    items.push(item);
                    added += 1;
                } else {
                    hidden += 1;
                }
            }
            cursor = page.next_cursor;
            if cursor.is_none() || added >= self.page_size as usize || pages >= self.max_pages {
                break;
            }
        }
        let bytes = 256
            + model::path_bytes(&key.parent)
            + cursor.as_ref().map_or(0, String::len)
            + items.iter().map(model::object_bytes).sum::<usize>();
        Ok(CacheValue::Children(Arc::new(NodeChildren {
            key: key.clone(),
            items,
            complete: cursor.is_none(),
            hidden_by_filter: hidden,
            loaded_at: Instant::now(),
            next_cursor: cursor,
            bytes,
        })))
    }
}

// ---------------------------------------------------------------------------------------------
// The service

/// Navigator metadata for the UI and the editor. Cheap to clone; every method returns at once.
#[derive(Clone)]
pub struct MetadataService {
    inner: Arc<Inner>,
}

impl fmt::Debug for MetadataService {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MetadataService")
            .field("connections", &lock(&self.inner.state).conns.len())
            .finish_non_exhaustive()
    }
}

impl MetadataService {
    /// A service over the manager's sessions, without stored filters (filters set with
    /// [`Self::set_filters`] are kept in memory only).
    #[must_use]
    pub fn new(manager: SessionManager, config: MetadataConfig) -> Self {
        Self::build(manager, config, None, NavigatorFilters::default())
    }

    /// A service whose filters are read from (and saved to) the project's `navigator.json`.
    /// **Blocking** (reads the file): call it off the UI thread ([`crate::CoreServices`] does).
    /// A missing file means no filter; entries that are invalid are dropped and returned.
    ///
    /// # Errors
    /// The file cannot be read or is from a newer NineSQL.
    pub fn with_workspace(
        manager: SessionManager,
        config: MetadataConfig,
        workspace: Workspace,
    ) -> Result<(Self, Vec<FilterIssue>), crate::WorkspaceError> {
        let (filters, issues) = workspace.load_navigator_filters()?;
        Ok((
            Self::build(manager, config, Some(workspace), filters),
            issues,
        ))
    }

    fn build(
        manager: SessionManager,
        config: MetadataConfig,
        workspace: Option<Workspace>,
        filters: NavigatorFilters,
    ) -> Self {
        let config = config.clamped();
        let (events, rx) = mpsc::channel(config.event_queue);
        Self {
            inner: Arc::new(Inner {
                manager,
                config,
                state: Mutex::new(State {
                    conns: HashMap::new(),
                    filters,
                }),
                events,
                events_rx: Mutex::new(Some(MetadataEvents { rx })),
                dropped: AtomicU64::new(0),
                next_id: AtomicU64::new(1),
                workspace,
            }),
        }
    }

    /// The event channel; the first call takes it.
    #[must_use]
    pub fn take_events(&self) -> Option<MetadataEvents> {
        lock(&self.inner.events_rx).take()
    }

    /// Events dropped because the channel was full.
    #[must_use]
    pub fn dropped_events(&self) -> u64 {
        self.inner.dropped.load(Ordering::Relaxed)
    }

    /// The configuration in effect (clamped).
    #[must_use]
    pub fn config(&self) -> &MetadataConfig {
        &self.inner.config
    }

    /// Kinds listed under `parent` (the root for top-level objects), in display order: what the
    /// driver lists there and the session declares (I8). Never blocks.
    ///
    /// # Errors
    /// [`MetadataError::NoSession`] when the connection is not open.
    pub fn child_kinds(
        &self,
        connection: ConnectionId,
        parent: &ObjectPath,
    ) -> Result<Vec<ObjectKind>, MetadataError> {
        let mut st = lock(&self.inner.state);
        let cs = self.inner.bind(&mut st, connection)?;
        let declared = &cs.caps.kinds;
        Ok(cs
            .metadata
            .child_kinds(parent.kind())
            .into_iter()
            .filter(|k| declared.contains(k))
            .collect())
    }

    /// Whether the session can produce DDL for `kind` (I8). `false` when the connection is not
    /// open.
    #[must_use]
    pub fn supports_ddl(&self, connection: ConnectionId, kind: ObjectKind) -> bool {
        let mut st = lock(&self.inner.state);
        self.inner
            .bind(&mut st, connection)
            .is_ok_and(|cs| cs.caps.ddl.contains(&kind))
    }

    /// The children of a node: from the cache when there, else loaded on the driver runtime
    /// (sharing a load already in flight). Dropping the request cancels it.
    pub fn children(
        &self,
        connection: ConnectionId,
        key: NodeKey,
    ) -> MetadataRequest<Arc<NodeChildren>> {
        self.inner
            .request(connection, FlightKey::Children(key), as_children)
    }

    /// Loads the next pages of an incomplete node and answers with the whole node (a complete
    /// or uncached node answers like [`Self::children`]).
    pub fn load_more(
        &self,
        connection: ConnectionId,
        key: NodeKey,
    ) -> MetadataRequest<Arc<NodeChildren>> {
        self.inner
            .request(connection, FlightKey::More(key), as_children)
    }

    /// Drops a node and everything loaded under its objects, then loads it again.
    pub fn refresh(
        &self,
        connection: ConnectionId,
        key: NodeKey,
    ) -> MetadataRequest<Arc<NodeChildren>> {
        self.invalidate_node(connection, &key, InvalidationReason::Refresh);
        self.children(connection, key)
    }

    fn invalidate_node(&self, connection: ConnectionId, key: &NodeKey, reason: InvalidationReason) {
        let mut st = lock(&self.inner.state);
        let Some(cs) = st.conns.get_mut(&connection) else {
            return;
        };
        let depth = key.parent.0.len();
        let below = |p: &ObjectPath| {
            p.0.len() > depth && p.0[depth].kind == key.kind && model::is_under(p, &key.parent)
        };
        let node = CacheKey::Children(key.clone());
        cs.cache.remove(&node);
        cs.cache.remove_where(|k| match k {
            CacheKey::Children(n) => below(&n.parent),
            CacheKey::Ddl(p) => below(p),
        });
        cs.mark_stale(|k| match k {
            FlightKey::Children(n) | FlightKey::More(n) => n == key || below(&n.parent),
            FlightKey::Ddl(p) => below(p),
        });
        drop(st);
        self.inner.emit(MetadataEvent::Invalidated {
            connection,
            scope: InvalidationScope::Node(key.clone()),
            reason,
        });
    }

    /// Drops what is cached at or under `scope` (the root: everything of the connection).
    /// Loads in flight there are not cached when they end.
    pub fn invalidate(&self, connection: ConnectionId, scope: &ObjectPath) {
        let mut st = lock(&self.inner.state);
        let Some(cs) = st.conns.get_mut(&connection) else {
            return;
        };
        cs.cache.remove_under(scope);
        cs.mark_stale(|k| k.cache_key().is_under(scope));
        drop(st);
        self.inner.emit(MetadataEvent::Invalidated {
            connection,
            scope: if scope.0.is_empty() {
                InvalidationScope::Connection
            } else {
                InvalidationScope::Subtree(scope.clone())
            },
            reason: InvalidationReason::Requested,
        });
    }

    /// The cached children of a node, if any. Never loads; never blocks.
    #[must_use]
    pub fn cached_children(
        &self,
        connection: ConnectionId,
        key: &NodeKey,
    ) -> Option<Arc<NodeChildren>> {
        let mut st = lock(&self.inner.state);
        let cs = st.conns.get_mut(&connection)?;
        cs.cache
            .get(&CacheKey::Children(key.clone()))
            .and_then(as_children)
    }

    /// The DDL of an object (F-NAV-07), kept in the cache with the node entries. Dropping the
    /// request cancels it.
    pub fn ddl(
        &self,
        connection: ConnectionId,
        path: ObjectPath,
    ) -> MetadataRequest<Arc<ObjectDdl>> {
        self.inner.request(connection, FlightKey::Ddl(path), as_ddl)
    }

    /// The cached DDL of an object, if any.
    #[must_use]
    pub fn cached_ddl(
        &self,
        connection: ConnectionId,
        path: &ObjectPath,
    ) -> Option<Arc<ObjectDdl>> {
        let mut st = lock(&self.inner.state);
        let cs = st.conns.get_mut(&connection)?;
        cs.cache.get(&CacheKey::Ddl(path.clone())).and_then(as_ddl)
    }

    /// Size of a connection's cache, `None` when nothing is bound.
    #[must_use]
    pub fn cache_stats(&self, connection: ConnectionId) -> Option<CacheStats> {
        let st = lock(&self.inner.state);
        let cs = st.conns.get(&connection)?;
        Some(CacheStats {
            session: cs.session.id(),
            entries: cs.cache.len(),
            bytes: cs.cache.bytes(),
            evictions: cs.cache.evictions(),
            generation: cs.cache.generation(),
            in_flight: cs.flights.len(),
        })
    }

    // -- Filters (F-NAV-04) -------------------------------------------------------------------

    /// The navigator filters of a connection.
    #[must_use]
    pub fn filters(&self, connection: ConnectionId) -> ConnectionFilters {
        lock(&self.inner.state).filters.of(connection)
    }

    /// Replaces a connection's filters: saved to `navigator.json` (when the service has a
    /// workspace) on the blocking pool, then applied, which drops the connection's cache
    /// ([`InvalidationReason::FiltersChanged`]). Nothing changes when the save fails.
    pub fn set_filters(
        &self,
        connection: ConnectionId,
        filters: ConnectionFilters,
    ) -> Reply<Result<(), MetadataError>> {
        let (tx, reply) = Reply::channel();
        let inner = Arc::clone(&self.inner);
        let handle = self.inner.manager.runtime().handle().clone();
        handle.spawn_blocking(move || {
            let result = (|| {
                let _write = lock(&NAVIGATOR_WRITE);
                let mut next = lock(&inner.state).filters.clone();
                next.set(connection, filters);
                if let Some(ws) = &inner.workspace {
                    ws.save_navigator_filters(&next)
                        .map_err(|e| MetadataError::Save(e.to_string()))?;
                }
                let mut st = lock(&inner.state);
                st.filters = next;
                if let Some(cs) = st.conns.get_mut(&connection) {
                    cs.cache.remove_under(&ObjectPath::root());
                    cs.mark_stale(|_| true);
                }
                drop(st);
                inner.emit(MetadataEvent::Invalidated {
                    connection,
                    scope: InvalidationScope::Connection,
                    reason: InvalidationReason::FiltersChanged,
                });
                Ok(())
            })();
            let _ = tx.send(result);
        });
        reply
    }

    // -- Search (F-NAV-05) --------------------------------------------------------------------

    /// Searches the cached metadata of the requested connections (never loads anything), on
    /// the runtime's blocking pool. Bounded by [`search::MAX_SEARCH_TIME`],
    /// [`search::MAX_SEARCH_CANDIDATES`] and [`search::MAX_SEARCH_RESULTS`].
    pub fn search(&self, request: SearchRequest) -> Reply<SearchResults> {
        let sources = self.search_sources(&request);
        let (tx, reply) = Reply::channel();
        self.inner
            .manager
            .runtime()
            .handle()
            .spawn_blocking(move || {
                let _ = tx.send(search::search(&request, &sources));
            });
        reply
    }

    fn search_sources(&self, request: &SearchRequest) -> Vec<(ConnectionId, CacheValue)> {
        let st = lock(&self.inner.state);
        let mut ids: Vec<ConnectionId> = if request.connections.is_empty() {
            st.conns.keys().copied().collect()
        } else {
            request.connections.clone()
        };
        ids.sort();
        ids.dedup();
        let mut out = Vec::new();
        for id in ids {
            if let Some(cs) = st.conns.get(&id) {
                out.extend(cs.cache.values().map(|v| (id, v.clone())));
            }
        }
        out
    }

    // -- Prefetch -----------------------------------------------------------------------------

    /// Loads, in the background, the nodes under `root` breadth-first, within `limits`, one
    /// request at a time through the cache. Explicit only (nothing prefetches by default);
    /// dropping the returned handle cancels it.
    #[must_use]
    pub fn prefetch(
        &self,
        connection: ConnectionId,
        root: ObjectPath,
        limits: PrefetchLimits,
    ) -> Prefetch {
        let limits = limits.clamped();
        let service = self.clone();
        let (tx, summary) = Reply::channel();
        let task = self.inner.manager.runtime().spawn(async move {
            let s = service.run_prefetch(connection, root, &limits).await;
            tracing::debug!(
                connection = %connection,
                loaded = s.loaded,
                failed = s.failed,
                truncated = s.truncated,
                "metadata prefetch ended"
            );
            let _ = tx.send(s);
        });
        Prefetch {
            abort: Some(task.abort_handle()),
            summary,
        }
    }

    /// Warms a schema for completion: its objects, then the columns of up to
    /// [`PrefetchLimits::max_nodes`] of them ([`Self::prefetch`] with depth 2).
    #[must_use]
    pub fn warm_schema(&self, connection: ConnectionId, schema: ObjectPath) -> Prefetch {
        self.prefetch(connection, schema, PrefetchLimits::default())
    }

    async fn run_prefetch(
        &self,
        connection: ConnectionId,
        root: ObjectPath,
        limits: &PrefetchLimits,
    ) -> PrefetchSummary {
        let mut s = PrefetchSummary::default();
        let mut queue = VecDeque::from([(root, 0usize)]);
        'outer: while let Some((path, depth)) = queue.pop_front() {
            let kinds = match self.child_kinds(connection, &path) {
                Ok(k) => k,
                Err(_) => {
                    s.failed += 1;
                    break;
                }
            };
            for kind in kinds {
                if limits.kinds.as_ref().is_some_and(|ks| !ks.contains(&kind)) {
                    continue;
                }
                if s.loaded + s.failed >= limits.max_nodes {
                    s.truncated = true;
                    break 'outer;
                }
                match self
                    .children(connection, NodeKey::new(path.clone(), kind))
                    .await
                {
                    Ok(node) => {
                        s.loaded += 1;
                        if depth + 1 < limits.max_depth {
                            for item in &node.items {
                                if queue.len() >= limits.max_nodes {
                                    s.truncated = true;
                                    break;
                                }
                                queue.push_back((item.path.clone(), depth + 1));
                            }
                        }
                    }
                    Err(MetadataError::SessionClosed | MetadataError::NoSession) => {
                        s.failed += 1;
                        break 'outer;
                    }
                    Err(_) => s.failed += 1,
                }
            }
        }
        s
    }

    // -- Completion ---------------------------------------------------------------------------

    /// What completion can offer for a connection, from the cache only: schemas, objects,
    /// columns and foreign keys loaded so far. Never loads and never blocks on I/O; rebuilt
    /// only when the cache changed (compare [`CompletionSnapshot::generation`]). `None` when
    /// nothing is bound for the connection. Ask [`Self::warm_schema`] to fill it.
    #[must_use]
    pub fn completion_snapshot(&self, connection: ConnectionId) -> Option<Arc<CompletionSnapshot>> {
        let (generation, sources) = {
            let st = lock(&self.inner.state);
            let cs = st.conns.get(&connection)?;
            let generation = cs.cache.generation();
            if let Some(s) = &cs.snapshot
                && s.generation == generation
            {
                return Some(Arc::clone(s));
            }
            let sources: Vec<Arc<NodeChildren>> = cs
                .cache
                .values()
                .filter_map(|v| match v {
                    CacheValue::Children(c) => Some(Arc::clone(c)),
                    CacheValue::Ddl(_) => None,
                })
                .collect();
            (generation, sources)
        };
        let snapshot = Arc::new(CompletionSnapshot::build(connection, generation, &sources));
        let mut st = lock(&self.inner.state);
        if let Some(cs) = st.conns.get_mut(&connection)
            && cs.cache.generation() == generation
        {
            cs.snapshot = Some(Arc::clone(&snapshot));
        }
        Some(snapshot)
    }
}

// ---------------------------------------------------------------------------------------------
// Prefetch handle

/// Limits of a background prefetch.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PrefetchLimits {
    /// Most nodes requested (1..=[`Self::MAX_NODES`]). Default 256.
    pub max_nodes: usize,
    /// Levels under the root (1..=[`Self::MAX_DEPTH`]). Default 2 (objects, then columns).
    pub max_depth: usize,
    /// Kinds to load; `None`: every declared kind.
    pub kinds: Option<BTreeSet<ObjectKind>>,
}

impl PrefetchLimits {
    /// Upper bound of [`Self::max_nodes`].
    pub const MAX_NODES: usize = 2_048;
    /// Upper bound of [`Self::max_depth`].
    pub const MAX_DEPTH: usize = 6;

    /// Limits with `max_nodes` and `max_depth`, every kind.
    #[must_use]
    pub fn new(max_nodes: usize, max_depth: usize) -> Self {
        Self {
            max_nodes,
            max_depth,
            kinds: None,
        }
    }

    /// Only these kinds.
    #[must_use]
    pub fn of_kinds(mut self, kinds: impl IntoIterator<Item = ObjectKind>) -> Self {
        self.kinds = Some(kinds.into_iter().collect());
        self
    }

    fn clamped(mut self) -> Self {
        self.max_nodes = self.max_nodes.clamp(1, Self::MAX_NODES);
        self.max_depth = self.max_depth.clamp(1, Self::MAX_DEPTH);
        self
    }
}

impl Default for PrefetchLimits {
    fn default() -> Self {
        Self::new(256, 2)
    }
}

/// What a prefetch did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct PrefetchSummary {
    /// Nodes loaded (or found in the cache).
    pub loaded: usize,
    /// Nodes that failed.
    pub failed: usize,
    /// Stopped at a limit.
    pub truncated: bool,
}

/// A running prefetch. **Dropping it cancels it**; [`Self::detach`] lets it finish (it stays
/// bounded by its limits).
pub struct Prefetch {
    abort: Option<AbortHandle>,
    summary: Reply<PrefetchSummary>,
}

impl fmt::Debug for Prefetch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Prefetch")
            .field("detached", &self.abort.is_none())
            .finish_non_exhaustive()
    }
}

impl Prefetch {
    /// Lets the prefetch run to its end; the returned reply answers with its summary.
    #[must_use]
    pub fn detach(mut self) -> Reply<PrefetchSummary> {
        self.abort = None;
        std::mem::replace(
            &mut self.summary,
            Reply::from_receiver(oneshot::channel().1),
        )
    }

    /// The summary, if it ended. Never blocks.
    pub fn try_take(&mut self) -> Next<PrefetchSummary> {
        self.summary.try_take()
    }

    /// Stops it now.
    pub fn cancel(self) {}
}

impl Future for Prefetch {
    type Output = Option<PrefetchSummary>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.summary).poll(cx)
    }
}

impl Drop for Prefetch {
    fn drop(&mut self) {
        if let Some(a) = self.abort.take() {
            a.abort();
        }
    }
}
