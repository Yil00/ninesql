//! Metadata service (P4-B core) against a scripted metadata driver: laziness, single flight,
//! cancel on drop, timeout, cache bounds and eviction, invalidation on session close, refresh,
//! filters (F-NAV-04), paging, capability checks (I8), search (F-NAV-05), completion snapshots
//! and prefetch.
#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).

mod support;

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ninesql_core::driver_api::metadata::{
    ColumnInfo, Metadata, ObjectDetails, ObjectInfo, ObjectPath, Page, PageRequest, TableInfo,
};
use ninesql_core::driver_api::{
    self as api, CancelHandle, Capabilities, ConnectParams, Dialect, Driver, DriverError,
    DriverResult, ExecOptions, ObjectKind, ResultStream, Session, Statement,
};
use ninesql_core::metadata::{KindFilter, MatchField};
use ninesql_core::session::Next;
use ninesql_core::{
    ConnectionFilters, ConnectionId, ConnectionTypeId, ConnectionTypes, InvalidationReason,
    InvalidationScope, MetadataConfig, MetadataError, MetadataEvent, MetadataService, NodeKey,
    PrefetchLimits, SearchRequest, SessionHandle, SessionManager,
};
use support::*;

// ---------------------------------------------------------------------------------------------
// A scripted metadata driver

#[derive(Default)]
struct Fake {
    nodes: Mutex<HashMap<(ObjectPath, ObjectKind), Vec<ObjectInfo>>>,
    list_calls: AtomicU64,
    ddl_calls: AtomicU64,
    /// Futures dropped before they finished (abandoned calls).
    abandoned: AtomicU64,
    delay_ms: AtomicU64,
    hang: AtomicBool,
    fail: AtomicBool,
    /// Adds an object from another parent to every answer (a misbehaving driver).
    stray: AtomicBool,
    filters_seen: Mutex<Vec<Option<String>>>,
}

struct Abandon<'a>(&'a AtomicU64, bool);
impl Drop for Abandon<'_> {
    fn drop(&mut self) {
        if !self.1 {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
}

fn schema(name: &str) -> ObjectPath {
    ObjectPath::root().child(ObjectKind::Schema, name)
}

fn table(s: &str, t: &str) -> ObjectPath {
    schema(s).child(ObjectKind::Table, t)
}

impl Fake {
    fn add(&self, parent: &ObjectPath, kind: ObjectKind, names: &[&str]) {
        let items = names
            .iter()
            .enumerate()
            .map(|(i, n)| ObjectInfo {
                path: parent.child(kind, *n),
                comment: None,
                details: match kind {
                    ObjectKind::Column => ObjectDetails::Column(ColumnInfo::new(
                        u32::try_from(i).unwrap() + 1,
                        "integer",
                    )),
                    ObjectKind::Table => ObjectDetails::Table(TableInfo::default()),
                    _ => ObjectDetails::None,
                },
            })
            .collect();
        self.nodes
            .lock()
            .unwrap()
            .insert((parent.clone(), kind), items);
    }

    fn comment(&self, parent: &ObjectPath, kind: ObjectKind, name: &str, comment: &str) {
        let mut nodes = self.nodes.lock().unwrap();
        let items = nodes.get_mut(&(parent.clone(), kind)).unwrap();
        let item = items.iter_mut().find(|i| i.name() == name).unwrap();
        item.comment = Some(comment.to_owned());
    }

    /// The "nine"-like tree: schemas `public`, `audit`; tables with columns and a foreign key.
    fn standard() -> Arc<Self> {
        let f = Arc::new(Self::default());
        f.add(
            &ObjectPath::root(),
            ObjectKind::Schema,
            &["audit", "public"],
        );
        f.add(
            &schema("public"),
            ObjectKind::Table,
            &["customers", "order_items", "orders", "tmp_import"],
        );
        f.add(&schema("public"), ObjectKind::View, &["customer_orders"]);
        f.add(&schema("audit"), ObjectKind::Table, &["audit_log"]);
        for t in ["customers", "order_items", "orders", "tmp_import"] {
            f.add(&table("public", t), ObjectKind::Column, &["id", "label"]);
        }
        f.add(
            &table("audit", "audit_log"),
            ObjectKind::Column,
            &["id", "at"],
        );
        f.add(
            &table("public", "orders"),
            ObjectKind::ForeignKey,
            &["orders_customer_fk"],
        );
        f.comment(
            &schema("public"),
            ObjectKind::Table,
            "customers",
            "People who buy things",
        );
        f
    }

    async fn pause(&self) {
        if self.hang.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
        let ms = self.delay_ms.load(Ordering::SeqCst);
        if ms > 0 {
            tokio::time::sleep(Duration::from_millis(ms)).await;
        }
    }
}

#[async_trait::async_trait]
impl Metadata for Fake {
    fn child_kinds(&self, parent: Option<ObjectKind>) -> Vec<ObjectKind> {
        match parent {
            None => vec![ObjectKind::Schema],
            // `Index` is listed here but not declared in the capabilities: the service hides it.
            Some(ObjectKind::Schema) => {
                vec![ObjectKind::Table, ObjectKind::View, ObjectKind::Index]
            }
            Some(ObjectKind::Table) => vec![ObjectKind::Column, ObjectKind::ForeignKey],
            Some(ObjectKind::View) => vec![ObjectKind::Column],
            Some(_) => Vec::new(),
        }
    }

    async fn list(
        &self,
        parent: &ObjectPath,
        kind: ObjectKind,
        page: PageRequest,
    ) -> DriverResult<Page<ObjectInfo>> {
        self.list_calls.fetch_add(1, Ordering::SeqCst);
        self.filters_seen.lock().unwrap().push(page.filter.clone());
        let mut guard = Abandon(&self.abandoned, false);
        self.pause().await;
        guard.1 = true;
        if self.fail.load(Ordering::SeqCst) {
            return Err(DriverError::Query {
                code: Some("42501".into()),
                message: "permission denied for schema audit".into(),
                position: None,
            });
        }
        let all: Vec<ObjectInfo> = self
            .nodes
            .lock()
            .unwrap()
            .get(&(parent.clone(), kind))
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|i| page.filter.as_deref().is_none_or(|f| i.name().contains(f)))
            .collect();
        let start: usize = page.cursor.as_deref().map_or(0, |c| c.parse().unwrap());
        let end = (start + page.limit as usize).min(all.len());
        let mut items = all[start..end].to_vec();
        if self.stray.load(Ordering::SeqCst) {
            items.push(ObjectInfo {
                path: schema("elsewhere").child(kind, "stray"),
                comment: None,
                details: ObjectDetails::None,
            });
        }
        Ok(Page {
            items,
            next_cursor: (end < all.len()).then(|| end.to_string()),
        })
    }

    async fn ddl(&self, object: &ObjectPath) -> DriverResult<String> {
        self.ddl_calls.fetch_add(1, Ordering::SeqCst);
        let mut guard = Abandon(&self.abandoned, false);
        self.pause().await;
        guard.1 = true;
        Ok(format!(
            "CREATE TABLE {} (id integer REFERENCES customers)",
            object.leaf().unwrap().name
        ))
    }
}

/// Wraps the mock driver; every session serves `fake` as its metadata.
struct FakeDriver {
    mock: ninesql_core::driver_api::mock::MockDriver,
    fake: Arc<Fake>,
}

fn capabilities() -> Capabilities {
    let mut caps = mock_capabilities();
    caps.metadata.kinds = BTreeSet::from([
        ObjectKind::Schema,
        ObjectKind::Table,
        ObjectKind::View,
        ObjectKind::Column,
        ObjectKind::ForeignKey,
    ]);
    caps.metadata.ddl = BTreeSet::from([ObjectKind::Table]);
    caps
}

#[async_trait::async_trait]
impl Driver for FakeDriver {
    fn family(&self) -> &'static str {
        "metadata-mock"
    }
    fn engines(&self) -> &[api::EngineId] {
        self.mock.engines()
    }
    fn capabilities(&self, engine: &api::EngineId, v: Option<&str>) -> DriverResult<Capabilities> {
        self.mock.capabilities(engine, v)
    }
    fn dialect(&self, engine: &api::EngineId) -> DriverResult<Arc<dyn Dialect>> {
        self.mock.dialect(engine)
    }
    async fn connect(&self, params: ConnectParams) -> DriverResult<Box<dyn Session>> {
        let inner = self.mock.connect(params).await?;
        Ok(Box::new(FakeSession {
            inner,
            fake: Arc::clone(&self.fake),
        }))
    }
}

struct FakeSession {
    inner: Box<dyn Session>,
    fake: Arc<Fake>,
}

#[async_trait::async_trait]
impl Session for FakeSession {
    fn capabilities(&self) -> &Capabilities {
        self.inner.capabilities()
    }
    fn server_version(&self) -> Option<&str> {
        self.inner.server_version()
    }
    fn dialect(&self) -> Arc<dyn Dialect> {
        self.inner.dialect()
    }
    fn metadata(&self) -> Arc<dyn Metadata> {
        Arc::clone(&self.fake) as Arc<dyn Metadata>
    }
    async fn execute(&mut self, s: Statement, o: ExecOptions) -> DriverResult<ResultStream> {
        self.inner.execute(s, o).await
    }
    fn cancel_handle(&self) -> Arc<dyn CancelHandle> {
        self.inner.cancel_handle()
    }
    fn auto_commit(&self) -> bool {
        self.inner.auto_commit()
    }
    fn in_transaction(&self) -> bool {
        self.inner.in_transaction()
    }
    async fn set_auto_commit(&mut self, on: bool) -> DriverResult<()> {
        self.inner.set_auto_commit(on).await
    }
    fn is_read_only(&self) -> bool {
        self.inner.is_read_only()
    }
    async fn set_read_only(&mut self, ro: bool) -> DriverResult<()> {
        self.inner.set_read_only(ro).await
    }
    async fn ping(&mut self) -> DriverResult<()> {
        self.inner.ping().await
    }
    async fn close(self: Box<Self>) -> DriverResult<()> {
        self.inner.close().await
    }
}

struct Env {
    _tmp: tempfile::TempDir,
    manager: SessionManager,
    fake: Arc<Fake>,
    types: ConnectionTypes,
}

fn env_with(fake: Arc<Fake>) -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let driver = FakeDriver {
        mock: ninesql_core::driver_api::mock::MockDriver::new().with_capabilities(capabilities()),
        fake: Arc::clone(&fake),
    };
    let (manager, _) = manager(Arc::new(driver), tmp.path());
    Env {
        _tmp: tmp,
        manager,
        fake,
        types: ConnectionTypes::default(),
    }
}

fn env() -> Env {
    env_with(Fake::standard())
}

impl Env {
    async fn open(&self) -> (ConnectionId, SessionHandle) {
        let c = mock_connection(ConnectionTypeId::dev());
        let id = c.id;
        (id, open_ok(&self.manager, c, &self.types).await)
    }

    fn calls(&self) -> u64 {
        self.fake.list_calls.load(Ordering::SeqCst)
    }
}

async fn within<F: std::future::Future>(f: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(10), f)
        .await
        .expect("timed out")
}

fn tables(s: &str) -> NodeKey {
    NodeKey::new(schema(s), ObjectKind::Table)
}

fn names(node: &ninesql_core::NodeChildren) -> Vec<&str> {
    node.items.iter().map(ObjectInfo::name).collect()
}

async fn wait_for(
    events: &mut ninesql_core::MetadataEvents,
    pred: impl Fn(&MetadataEvent) -> bool,
) -> MetadataEvent {
    within(async {
        loop {
            let e = events.recv().await.expect("events ended");
            if pred(&e) {
                return e;
            }
        }
    })
    .await
}

// ---------------------------------------------------------------------------------------------
// Tests

#[test]
fn loading_is_lazy_cached_and_reported() {
    let env = env();
    let svc = MetadataService::new(env.manager.clone(), MetadataConfig::default());
    let mut events = svc.take_events().unwrap();
    assert!(svc.take_events().is_none());
    ui(async {
        let (id, _h) = env.open().await;
        // Opening and asking for kinds lists nothing.
        let kinds = svc.child_kinds(id, &schema("public")).unwrap();
        assert_eq!(kinds, vec![ObjectKind::Table, ObjectKind::View]);
        assert_eq!(env.calls(), 0);

        let key = NodeKey::new(ObjectPath::root(), ObjectKind::Schema);
        let first = within(svc.children(id, key.clone())).await.unwrap();
        assert_eq!(names(&first), ["audit", "public"]);
        assert!(first.complete);
        assert_eq!(env.calls(), 1);
        let again = within(svc.children(id, key.clone())).await.unwrap();
        assert!(Arc::ptr_eq(&first, &again));
        assert_eq!(env.calls(), 1);
        assert!(svc.cached_children(id, &key).is_some());
        assert!(svc.cached_children(id, &tables("public")).is_none());

        let e = wait_for(&mut events, |e| {
            matches!(e, MetadataEvent::NodeLoaded { .. })
        })
        .await;
        assert_eq!(
            e,
            MetadataEvent::NodeLoaded {
                connection: id,
                key,
                items: 2,
                complete: true,
                hidden_by_filter: 0,
                cached: true,
            }
        );
    });
}

#[test]
fn the_same_node_loading_twice_shares_one_request() {
    let env = env();
    env.fake.delay_ms.store(200, Ordering::SeqCst);
    let svc = MetadataService::new(env.manager.clone(), MetadataConfig::default());
    ui(async {
        let (id, _h) = env.open().await;
        let a = svc.children(id, tables("public"));
        let b = svc.children(id, tables("public"));
        assert_eq!(svc.cache_stats(id).unwrap().in_flight, 1);
        let (a, b) = within(async { tokio::join!(a, b) }).await;
        assert!(Arc::ptr_eq(&a.unwrap(), &b.unwrap()));
        assert_eq!(env.calls(), 1);
        // A different node is its own request.
        let _ = within(svc.children(id, tables("audit"))).await.unwrap();
        assert_eq!(env.calls(), 2);
    });
}

#[test]
fn dropping_the_last_waiter_cancels_the_driver_call() {
    let env = env();
    env.fake.hang.store(true, Ordering::SeqCst);
    let svc = MetadataService::new(env.manager.clone(), MetadataConfig::default());
    ui(async {
        let (id, _h) = env.open().await;
        let a = svc.children(id, tables("public"));
        let mut b = svc.children(id, tables("public"));
        within(async {
            while env.calls() == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        // One waiter left: the call goes on.
        drop(a);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(svc.cache_stats(id).unwrap().in_flight, 1);
        assert_eq!(env.fake.abandoned.load(Ordering::SeqCst), 0);
        assert!(matches!(b.try_take(), Next::Pending));
        // The last one: the driver future is dropped.
        drop(b);
        within(async {
            while env.fake.abandoned.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        assert_eq!(svc.cache_stats(id).unwrap().in_flight, 0);
        assert!(svc.cached_children(id, &tables("public")).is_none());
    });
}

#[test]
fn a_request_that_takes_too_long_times_out() {
    let env = env();
    env.fake.hang.store(true, Ordering::SeqCst);
    let mut config = MetadataConfig::default();
    config.request_timeout = Duration::from_millis(150);
    let svc = MetadataService::new(env.manager.clone(), config);
    let mut events = svc.take_events().unwrap();
    ui(async {
        let (id, _h) = env.open().await;
        let r = within(svc.children(id, tables("public"))).await;
        assert_eq!(
            r.unwrap_err(),
            MetadataError::Timeout(Duration::from_millis(150))
        );
        let e = wait_for(&mut events, |e| {
            matches!(e, MetadataEvent::NodeFailed { .. })
        })
        .await;
        let MetadataEvent::NodeFailed { error, .. } = e else {
            unreachable!()
        };
        assert_eq!(error.kind(), "timeout");
        let r = within(svc.ddl(id, table("public", "orders"))).await;
        assert!(matches!(r, Err(MetadataError::Timeout(_))));
        // Nothing was cached; the next request tries again.
        env.fake.hang.store(false, Ordering::SeqCst);
        assert!(within(svc.children(id, tables("public"))).await.is_ok());
    });
}

#[test]
fn driver_errors_are_presented_and_not_cached() {
    let env = env();
    env.fake.fail.store(true, Ordering::SeqCst);
    let svc = MetadataService::new(env.manager.clone(), MetadataConfig::default());
    ui(async {
        let (id, _h) = env.open().await;
        let e = within(svc.children(id, tables("audit"))).await.unwrap_err();
        assert_eq!(e.kind(), "driver-query");
        assert!(e.to_string().contains("permission denied"));
        env.fake.fail.store(false, Ordering::SeqCst);
        assert!(within(svc.children(id, tables("audit"))).await.is_ok());
        assert_eq!(env.calls(), 2);
    });
}

#[test]
fn undeclared_kinds_and_ddl_never_reach_the_driver() {
    let env = env();
    let svc = MetadataService::new(env.manager.clone(), MetadataConfig::default());
    ui(async {
        let (id, _h) = env.open().await;
        let e = within(svc.children(id, NodeKey::new(schema("public"), ObjectKind::Index)))
            .await
            .unwrap_err();
        assert_eq!(e, MetadataError::Unsupported(ObjectKind::Index));
        // Declared, but not listed under this parent by the driver.
        let e = within(svc.children(id, NodeKey::new(ObjectPath::root(), ObjectKind::Table)))
            .await
            .unwrap_err();
        assert_eq!(e, MetadataError::Unsupported(ObjectKind::Table));
        assert_eq!(env.calls(), 0);

        let view = schema("public").child(ObjectKind::View, "customer_orders");
        assert_eq!(
            within(svc.ddl(id, view)).await.unwrap_err(),
            MetadataError::DdlUnsupported(Some(ObjectKind::View))
        );
        assert_eq!(env.fake.ddl_calls.load(Ordering::SeqCst), 0);
        assert!(svc.supports_ddl(id, ObjectKind::Table));
        assert!(!svc.supports_ddl(id, ObjectKind::View));

        let orders = table("public", "orders");
        let ddl = within(svc.ddl(id, orders.clone())).await.unwrap();
        assert!(ddl.text.starts_with("CREATE TABLE orders"));
        let again = within(svc.ddl(id, orders.clone())).await.unwrap();
        assert!(Arc::ptr_eq(&ddl, &again));
        assert_eq!(env.fake.ddl_calls.load(Ordering::SeqCst), 1);
        assert!(svc.cached_ddl(id, &orders).is_some());
        // Invalidating the table drops its DDL too.
        svc.invalidate(id, &orders);
        assert!(svc.cached_ddl(id, &orders).is_none());
    });
}

#[test]
fn no_session_means_no_request() {
    let env = env();
    let svc = MetadataService::new(env.manager.clone(), MetadataConfig::default());
    ui(async {
        let id = ConnectionId::new_random();
        assert_eq!(
            within(svc.children(id, tables("public")))
                .await
                .unwrap_err(),
            MetadataError::NoSession
        );
        assert_eq!(
            svc.child_kinds(id, &ObjectPath::root()).unwrap_err(),
            MetadataError::NoSession
        );
        assert!(svc.completion_snapshot(id).is_none());
    });
}

#[test]
fn the_cache_is_bounded_by_entries_and_bytes() {
    let env = env();
    let mut config = MetadataConfig::default();
    config.max_nodes_per_connection = 3;
    let svc = MetadataService::new(env.manager.clone(), config);
    ui(async {
        let (id, _h) = env.open().await;
        for t in ["customers", "order_items", "orders", "tmp_import"] {
            let key = NodeKey::new(table("public", t), ObjectKind::Column);
            within(svc.children(id, key)).await.unwrap();
        }
        within(svc.children(id, tables("public"))).await.unwrap();
        let stats = svc.cache_stats(id).unwrap();
        assert_eq!(stats.entries, 3);
        assert_eq!(stats.evictions, 2);
        // Least recently used first: the first two column nodes are gone.
        let first = NodeKey::new(table("public", "customers"), ObjectKind::Column);
        assert!(svc.cached_children(id, &first).is_none());
        let last = NodeKey::new(table("public", "tmp_import"), ObjectKind::Column);
        assert!(svc.cached_children(id, &last).is_some());
    });

    // A byte budget smaller than one node: the node is answered but not cached.
    let env = self::env();
    let mut config = MetadataConfig::default();
    config.max_bytes_per_connection = 200;
    let svc = MetadataService::new(env.manager.clone(), config);
    let mut events = svc.take_events().unwrap();
    ui(async {
        let (id, _h) = env.open().await;
        let node = within(svc.children(id, tables("public"))).await.unwrap();
        assert_eq!(node.items.len(), 4);
        assert!(svc.cached_children(id, &tables("public")).is_none());
        let e = wait_for(&mut events, |e| {
            matches!(e, MetadataEvent::NodeLoaded { .. })
        })
        .await;
        assert!(matches!(e, MetadataEvent::NodeLoaded { cached: false, .. }));
        assert!(svc.cache_stats(id).unwrap().bytes <= 200);
    });
}

#[test]
fn closing_the_session_clears_the_cache_and_fails_requests_in_flight() {
    let env = env();
    let svc = MetadataService::new(env.manager.clone(), MetadataConfig::default());
    let mut events = svc.take_events().unwrap();
    ui(async {
        let (id, h) = env.open().await;
        within(svc.children(id, tables("public"))).await.unwrap();
        assert_eq!(svc.cache_stats(id).unwrap().session, h.id());
        env.fake.hang.store(true, Ordering::SeqCst);
        let pending = svc.children(id, tables("audit"));
        within(h.close()).await.unwrap().unwrap();
        assert_eq!(
            within(pending).await.unwrap_err(),
            MetadataError::SessionClosed
        );
        let e = wait_for(&mut events, |e| {
            matches!(e, MetadataEvent::Invalidated { .. })
        })
        .await;
        assert_eq!(
            e,
            MetadataEvent::Invalidated {
                connection: id,
                scope: InvalidationScope::Connection,
                reason: InvalidationReason::SessionClosed,
            }
        );
        assert!(svc.cache_stats(id).is_none());
        assert!(svc.cached_children(id, &tables("public")).is_none());
        assert_eq!(
            within(svc.children(id, tables("public")))
                .await
                .unwrap_err(),
            MetadataError::NoSession
        );
        // A new session of the connection binds a fresh cache.
        env.fake.hang.store(false, Ordering::SeqCst);
        let c2 = {
            let mut c = mock_connection(ConnectionTypeId::dev());
            c.id = id;
            c
        };
        let h2 = open_ok(&env.manager, c2, &env.types).await;
        within(svc.children(id, tables("public"))).await.unwrap();
        assert_eq!(svc.cache_stats(id).unwrap().session, h2.id());
    });
}

#[test]
fn refresh_reloads_a_node_and_drops_what_lies_under_it() {
    let env = env();
    let svc = MetadataService::new(env.manager.clone(), MetadataConfig::default());
    let mut events = svc.take_events().unwrap();
    ui(async {
        let (id, _h) = env.open().await;
        let schemas = NodeKey::new(ObjectPath::root(), ObjectKind::Schema);
        within(svc.children(id, schemas.clone())).await.unwrap();
        within(svc.children(id, tables("public"))).await.unwrap();
        let cols = NodeKey::new(table("public", "orders"), ObjectKind::Column);
        within(svc.children(id, cols.clone())).await.unwrap();
        let views = NodeKey::new(schema("public"), ObjectKind::View);
        within(svc.children(id, views.clone())).await.unwrap();
        let calls = env.calls();

        env.fake.add(
            &schema("public"),
            ObjectKind::Table,
            &["customers", "invoices"],
        );
        let fresh = within(svc.refresh(id, tables("public"))).await.unwrap();
        assert_eq!(names(&fresh), ["customers", "invoices"]);
        assert_eq!(env.calls(), calls + 1);
        // Under the refreshed tables: dropped. Siblings and ancestors: kept.
        assert!(svc.cached_children(id, &cols).is_none());
        assert!(svc.cached_children(id, &views).is_some());
        assert!(svc.cached_children(id, &schemas).is_some());
        let e = wait_for(&mut events, |e| {
            matches!(e, MetadataEvent::Invalidated { .. })
        })
        .await;
        assert!(matches!(
            e,
            MetadataEvent::Invalidated {
                scope: InvalidationScope::Node(_),
                reason: InvalidationReason::Refresh,
                ..
            }
        ));

        // Invalidating a subtree: the schema and everything under it.
        svc.invalidate(id, &schema("public"));
        assert!(svc.cached_children(id, &tables("public")).is_none());
        assert!(svc.cached_children(id, &views).is_none());
        assert!(svc.cached_children(id, &schemas).is_some());
        svc.invalidate(id, &ObjectPath::root());
        assert_eq!(svc.cache_stats(id).unwrap().entries, 0);
    });
}

#[test]
fn a_refresh_during_a_load_does_not_keep_the_old_answer() {
    let env = env();
    env.fake.delay_ms.store(300, Ordering::SeqCst);
    let svc = MetadataService::new(env.manager.clone(), MetadataConfig::default());
    ui(async {
        let (id, _h) = env.open().await;
        let old = svc.children(id, tables("public"));
        tokio::time::sleep(Duration::from_millis(50)).await;
        env.fake
            .add(&schema("public"), ObjectKind::Table, &["fresh"]);
        let new = svc.refresh(id, tables("public"));
        let (old, new) = within(async { tokio::join!(old, new) }).await;
        // Both waiters get the reloaded node, and that is what the cache holds.
        assert_eq!(names(&old.unwrap()), ["fresh"]);
        assert_eq!(names(&new.unwrap()), ["fresh"]);
        assert_eq!(
            names(&svc.cached_children(id, &tables("public")).unwrap()),
            ["fresh"]
        );
        assert_eq!(env.fake.abandoned.load(Ordering::SeqCst), 1);
    });
}

#[test]
fn large_nodes_are_paged_and_loaded_on_demand() {
    let fake = Fake::standard();
    let many: Vec<String> = (0..50).map(|i| format!("t{i:02}")).collect();
    let refs: Vec<&str> = many.iter().map(String::as_str).collect();
    fake.add(&schema("public"), ObjectKind::Table, &refs);
    let env = env_with(fake);
    let mut config = MetadataConfig::default();
    config.page_size = 10;
    config.max_pages_per_load = 2;
    let svc = MetadataService::new(env.manager.clone(), config);
    ui(async {
        let (id, _h) = env.open().await;
        let first = within(svc.children(id, tables("public"))).await.unwrap();
        assert_eq!(first.items.len(), 10);
        assert!(!first.complete);
        let more = within(svc.load_more(id, tables("public"))).await.unwrap();
        assert_eq!(more.items.len(), 20);
        let mut node = more;
        while !node.complete {
            node = within(svc.load_more(id, tables("public"))).await.unwrap();
        }
        assert_eq!(node.items.len(), 50);
        assert_eq!(node.items[49].name(), "t49");
        assert_eq!(
            svc.cached_children(id, &tables("public"))
                .unwrap()
                .items
                .len(),
            50
        );
        let calls = env.calls();
        within(svc.load_more(id, tables("public"))).await.unwrap();
        assert_eq!(env.calls(), calls);
    });
}

#[test]
fn objects_from_elsewhere_in_an_answer_are_dropped() {
    let env = env();
    env.fake.stray.store(true, Ordering::SeqCst);
    let svc = MetadataService::new(env.manager.clone(), MetadataConfig::default());
    ui(async {
        let (id, _h) = env.open().await;
        let node = within(svc.children(id, tables("public"))).await.unwrap();
        assert!(!names(&node).contains(&"stray"));
        assert_eq!(node.items.len(), 4);
    });
}

#[test]
fn filters_apply_when_listing_and_are_stored_per_project() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = ninesql_storage::Paths::new(&ninesql_storage::XdgDirs {
        config_home: tmp.path().join("config"),
        data_home: tmp.path().join("data"),
        state_home: tmp.path().join("state"),
    });
    let workspace =
        ninesql_core::Workspace::open(paths, ninesql_storage::ProjectName::default_project())
            .unwrap();
    let env = env();
    let (svc, issues) = MetadataService::with_workspace(
        env.manager.clone(),
        MetadataConfig::default(),
        workspace.clone(),
    )
    .unwrap();
    assert!(issues.is_empty());
    let mut events = svc.take_events().unwrap();
    ui(async {
        let (id, _h) = env.open().await;
        within(svc.children(id, tables("public"))).await.unwrap();

        let filters = ConnectionFilters::new()
            .with(
                ObjectKind::Table,
                KindFilter::new(Vec::<&str>::new(), ["TMP_*"], false).unwrap(),
            )
            .unwrap();
        within(svc.set_filters(id, filters.clone()))
            .await
            .unwrap()
            .unwrap();
        let e = wait_for(&mut events, |e| {
            matches!(e, MetadataEvent::Invalidated { .. })
        })
        .await;
        assert!(matches!(
            e,
            MetadataEvent::Invalidated {
                reason: InvalidationReason::FiltersChanged,
                scope: InvalidationScope::Connection,
                ..
            }
        ));
        assert!(svc.cached_children(id, &tables("public")).is_none());
        let node = within(svc.children(id, tables("public"))).await.unwrap();
        assert_eq!(names(&node), ["customers", "order_items", "orders"]);
        assert_eq!(node.hidden_by_filter, 1);
        // Case-insensitive: never sent to the server.
        assert_eq!(env.fake.filters_seen.lock().unwrap().last().unwrap(), &None);
        // Other kinds are not filtered.
        let views = within(svc.children(id, NodeKey::new(schema("public"), ObjectKind::View)))
            .await
            .unwrap();
        assert_eq!(views.items.len(), 1);

        // Case-sensitive with one include: its literal goes to the server, the exact match
        // stays client-side.
        let filters = ConnectionFilters::new()
            .with(
                ObjectKind::Table,
                KindFilter::new(["order*s"], Vec::<&str>::new(), true).unwrap(),
            )
            .unwrap();
        within(svc.set_filters(id, filters.clone()))
            .await
            .unwrap()
            .unwrap();
        let node = within(svc.children(id, tables("public"))).await.unwrap();
        assert_eq!(names(&node), ["order_items", "orders"]);
        assert_eq!(
            env.fake
                .filters_seen
                .lock()
                .unwrap()
                .last()
                .unwrap()
                .as_deref(),
            Some("order")
        );
        assert_eq!(svc.filters(id), filters);

        // Stored in the project's navigator.json, read back by a new service.
        let (loaded, _) = workspace.load_navigator_filters().unwrap();
        assert_eq!(loaded.of(id), filters);
        let file = std::fs::read_to_string(workspace.project_paths().navigator_file()).unwrap();
        assert!(file.contains("\"schema_version\": 1"));
        assert!(file.contains("order*s"));
        let (again, _) = MetadataService::with_workspace(
            env.manager.clone(),
            MetadataConfig::default(),
            workspace.clone(),
        )
        .unwrap();
        assert_eq!(again.filters(id), filters);

        // Clearing them.
        within(svc.set_filters(id, ConnectionFilters::new()))
            .await
            .unwrap()
            .unwrap();
        let node = within(svc.children(id, tables("public"))).await.unwrap();
        assert_eq!(node.items.len(), 4);
        assert!(
            workspace
                .load_navigator_filters()
                .unwrap()
                .0
                .of(id)
                .is_empty()
        );
    });
}

#[test]
fn search_ranks_cached_objects_across_connections() {
    let env = env();
    let svc = MetadataService::new(env.manager.clone(), MetadataConfig::default());
    ui(async {
        let (a, _ha) = env.open().await;
        let (b, _hb) = env.open().await;
        for id in [a, b] {
            within(svc.children(id, tables("public"))).await.unwrap();
            within(svc.children(id, NodeKey::new(schema("public"), ObjectKind::View)))
                .await
                .unwrap();
        }
        within(svc.children(a, tables("audit"))).await.unwrap();
        let calls = env.calls();

        let r = within(svc.search(SearchRequest::new("orders")))
            .await
            .unwrap();
        assert!(!r.truncated);
        let top: Vec<(&str, ConnectionId)> =
            r.hits.iter().map(|h| (h.name(), h.connection)).collect();
        // Exact matches first (one per connection), then the word match.
        assert_eq!(top[0].0, "orders");
        assert_eq!(top[1].0, "orders");
        assert_ne!(top[0].1, top[1].1);
        assert_eq!(top[2].0, "customer_orders");
        assert_eq!(r.hits[0].positions, vec![0, 1, 2, 3, 4, 5]);

        // One connection, fuzzy, kinds.
        let r = within(svc.search(SearchRequest::new("oi").in_connections([a])))
            .await
            .unwrap();
        assert_eq!(r.hits[0].name(), "order_items");
        assert!(r.hits.iter().all(|h| h.connection == a));
        let r = within(svc.search(SearchRequest::new("orders").of_kinds([ObjectKind::View])))
            .await
            .unwrap();
        assert!(r.hits.iter().all(|h| h.kind() == Some(ObjectKind::View)));
        assert_eq!(r.hits.len(), 2);

        // Comments only when asked.
        let r = within(svc.search(SearchRequest::new("buy things")))
            .await
            .unwrap();
        assert!(r.hits.is_empty());
        let r = within(
            svc.search(
                SearchRequest::new("buy things")
                    .with_comments(true)
                    .in_connections([a]),
            ),
        )
        .await
        .unwrap();
        assert_eq!(r.hits.len(), 1);
        assert_eq!(r.hits[0].name(), "customers");
        assert_eq!(r.hits[0].field, MatchField::Comment);

        // Definitions, when loaded.
        within(svc.ddl(a, table("public", "order_items")))
            .await
            .unwrap();
        let r = within(svc.search(SearchRequest::new("references").with_definitions(true)))
            .await
            .unwrap();
        assert_eq!(r.hits.len(), 1);
        assert_eq!(r.hits[0].field, MatchField::Definition);

        // Limits.
        let r = within(svc.search(SearchRequest::new("o").with_limit(2)))
            .await
            .unwrap();
        assert_eq!(r.hits.len(), 2);
        // Searching never loads anything.
        assert_eq!(env.calls(), calls);
        let r = within(svc.search(SearchRequest::new("   "))).await.unwrap();
        assert!(r.hits.is_empty());
    });
}

#[test]
fn completion_snapshot_reads_the_cache_and_warm_schema_fills_it() {
    let env = env();
    let svc = MetadataService::new(env.manager.clone(), MetadataConfig::default());
    ui(async {
        let (id, _h) = env.open().await;
        within(svc.children(id, NodeKey::new(ObjectPath::root(), ObjectKind::Schema)))
            .await
            .unwrap();
        let snap = svc.completion_snapshot(id).unwrap();
        assert_eq!(snap.schemas(), ["audit", "public"]);
        assert!(snap.objects(Some("public")).is_empty());
        assert!(Arc::ptr_eq(&snap, &svc.completion_snapshot(id).unwrap()));

        let summary = within(svc.warm_schema(id, schema("public"))).await.unwrap();
        // Tables and views, then columns of 5 relations and the foreign keys of 4 tables.
        assert_eq!(summary.loaded, 2 + 5 + 4);
        assert_eq!(summary.failed, 0);
        assert!(!summary.truncated);
        let snap2 = svc.completion_snapshot(id).unwrap();
        assert!(snap2.generation > snap.generation);
        let objects: Vec<(&str, ObjectKind)> = snap2
            .objects(Some("public"))
            .iter()
            .map(|o| (o.name.as_str(), o.kind))
            .collect();
        assert!(objects.contains(&("orders", ObjectKind::Table)));
        assert!(objects.contains(&("customer_orders", ObjectKind::View)));
        let cols = snap2.columns(Some("public"), "orders").unwrap();
        assert_eq!(cols[0].name, "id");
        assert_eq!(cols[0].data_type.as_deref(), Some("integer"));
        assert!(snap2.columns(None, "orders").is_some());
        assert!(snap2.columns(Some("audit"), "audit_log").is_none());
        let fks = snap2.foreign_keys(Some("public"), "orders");
        assert_eq!(fks.len(), 1);
        assert_eq!(fks[0].name, "orders_customer_fk");
    });
}

#[test]
fn prefetch_is_bounded_and_cancelled_on_drop() {
    let env = env();
    let svc = MetadataService::new(env.manager.clone(), MetadataConfig::default());
    ui(async {
        let (id, _h) = env.open().await;
        let s = within(svc.prefetch(id, ObjectPath::root(), PrefetchLimits::new(3, 6)))
            .await
            .unwrap();
        assert_eq!(s.loaded, 3);
        assert!(s.truncated);
        assert_eq!(env.calls(), 3);

        let s = within(svc.prefetch(
            id,
            ObjectPath::root(),
            PrefetchLimits::new(100, 2).of_kinds([ObjectKind::Schema, ObjectKind::Table]),
        ))
        .await
        .unwrap();
        assert_eq!(s.loaded, 3); // schemas, then tables of audit and public

        env.fake.hang.store(true, Ordering::SeqCst);
        let before = env.calls();
        let p = svc.prefetch(
            id,
            schema("public"),
            PrefetchLimits::new(10, 1).of_kinds([ObjectKind::View]),
        );
        within(async {
            while env.calls() == before {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        drop(p);
        within(async {
            while env.fake.abandoned.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        assert_eq!(svc.cache_stats(id).unwrap().in_flight, 0);

        // A detached prefetch keeps running.
        env.fake.hang.store(false, Ordering::SeqCst);
        let reply = svc.warm_schema(id, schema("audit")).detach();
        let s = within(reply).await.unwrap();
        assert!(s.loaded >= 1);
    });
}

#[test]
fn concurrent_driver_calls_per_connection_are_limited() {
    let env = env();
    env.fake.hang.store(true, Ordering::SeqCst);
    let mut config = MetadataConfig::default();
    config.max_concurrent_requests = 2;
    let svc = MetadataService::new(env.manager.clone(), config);
    ui(async {
        let (id, _h) = env.open().await;
        let reqs: Vec<_> = ["customers", "order_items", "orders", "tmp_import"]
            .iter()
            .map(|t| svc.children(id, NodeKey::new(table("public", t), ObjectKind::Column)))
            .collect();
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(env.calls(), 2);
        assert_eq!(svc.cache_stats(id).unwrap().in_flight, 4);
        drop(reqs);
    });
}
