//! Shared helpers of the runtime tests: a manager around the mock driver, and a wrapping driver
//! that records what the core passed to `connect` and can simulate misbehaviour.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).

pub mod sshd;

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ninesql_core::driver_api::mock::{MOCK_ENGINE, MockDriver};
use ninesql_core::driver_api::{
    self as api, CancelHandle, Capabilities, ConnectParams, Dialect, Driver, DriverError,
    DriverResult, ExecOptions, Metadata, ResultStream, Session, Statement,
};
use ninesql_core::{
    Auth, Connection, ConnectionTypeId, ConnectionTypes, CoreError, DriverRegistry, Endpoint,
    EngineId, ExecEvent, ExecuteRequest, Execution, KnownHostsFiles, ManagerConfig, OpenOutcome,
    OpenRequest, SessionHandle, SessionManager,
};
use ninesql_secrets::{MemoryStore, SecretStores, StoreKind};

/// What the core handed to the driver, without secret values.
#[derive(Debug, Clone)]
pub struct Seen {
    pub endpoint: api::Endpoint,
    pub server_name: Option<String>,
    pub read_only: bool,
    pub auto_commit: bool,
    pub production: bool,
    pub tunnel: Option<api::TunnelInfo>,
    pub has_password: bool,
    pub property_keys: Vec<String>,
    pub init_statements: usize,
    pub inside_dbeaver_workspace: bool,
    pub password_exchange: api::PasswordExchange,
}

/// Wraps [`MockDriver`]: records connects, can strip the read-only flag (a broken driver), and
/// can keep its sessions busy until a deadline (a cancel the server has not confirmed yet).
#[derive(Clone)]
pub struct Wrap {
    pub mock: MockDriver,
    pub seen: Arc<Mutex<Vec<Seen>>>,
    pub ignore_read_only: bool,
    /// Also drops `inside_dbeaver_workspace` (a file driver that does not know the flag).
    pub ignore_dbeaver_flag: bool,
    pub busy_until: Arc<Mutex<Option<Instant>>>,
}

impl Wrap {
    pub fn new(mock: MockDriver) -> Self {
        Self {
            mock,
            seen: Arc::default(),
            ignore_read_only: false,
            ignore_dbeaver_flag: false,
            busy_until: Arc::default(),
        }
    }

    pub fn busy_for(&self, d: Duration) {
        *self.busy_until.lock().unwrap() = Some(Instant::now() + d);
    }

    pub fn last_seen(&self) -> Seen {
        self.seen.lock().unwrap().last().cloned().unwrap()
    }
}

#[async_trait::async_trait]
impl Driver for Wrap {
    fn family(&self) -> &'static str {
        "wrapped-mock"
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
    async fn connect(&self, mut params: ConnectParams) -> DriverResult<Box<dyn Session>> {
        self.seen.lock().unwrap().push(Seen {
            endpoint: params.endpoint.clone(),
            server_name: params.tls.server_name.clone(),
            read_only: params.read_only,
            auto_commit: params.auto_commit,
            production: params.production,
            tunnel: params.tunnel.clone(),
            has_password: matches!(
                &params.auth,
                api::Auth::Password {
                    password: Some(_),
                    ..
                }
            ),
            property_keys: params.properties.keys().cloned().collect(),
            init_statements: params.init_statements.len(),
            inside_dbeaver_workspace: params.inside_dbeaver_workspace,
            password_exchange: params.password_exchange,
        });
        if self.ignore_read_only {
            params.read_only = false;
        }
        if self.ignore_dbeaver_flag {
            params.inside_dbeaver_workspace = false;
        }
        let inner = self.mock.connect(params).await?;
        Ok(Box::new(WrapSession {
            inner,
            busy_until: Arc::clone(&self.busy_until),
        }))
    }
}

pub struct WrapSession {
    inner: Box<dyn Session>,
    busy_until: Arc<Mutex<Option<Instant>>>,
}

#[async_trait::async_trait]
impl Session for WrapSession {
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
        self.inner.metadata()
    }
    async fn execute(&mut self, s: Statement, o: ExecOptions) -> DriverResult<ResultStream> {
        let busy = self
            .busy_until
            .lock()
            .unwrap()
            .is_some_and(|until| Instant::now() < until);
        if busy {
            return Err(DriverError::Busy);
        }
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
    async fn begin(&mut self) -> DriverResult<()> {
        self.inner.begin().await
    }
    async fn commit(&mut self) -> DriverResult<()> {
        self.inner.commit().await
    }
    async fn rollback(&mut self) -> DriverResult<()> {
        self.inner.rollback().await
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

/// A manager with `driver` registered, a keyring in memory and `known_hosts` under `dir`.
pub fn manager(driver: Arc<dyn Driver>, dir: &Path) -> (SessionManager, Arc<MemoryStore>) {
    manager_with(driver, dir, |_| {})
}

/// [`manager`], with the configuration adjusted by `tune`.
pub fn manager_with(
    driver: Arc<dyn Driver>,
    dir: &Path,
    tune: impl FnOnce(&mut ManagerConfig),
) -> (SessionManager, Arc<MemoryStore>) {
    let mut registry = DriverRegistry::new();
    registry.register(driver);
    let keyring = Arc::new(MemoryStore::new(StoreKind::Keyring));
    let mut stores = SecretStores::new();
    stores.register(keyring.clone());
    let mut config = ManagerConfig::with_known_hosts(KnownHostsFiles {
        own: dir.join("known_hosts"),
        user: None,
    });
    config.workers = 2;
    config.connect_timeout = Duration::from_secs(5);
    config.busy_wait = Duration::from_millis(400);
    tune(&mut config);
    (
        SessionManager::new(config, registry, stores).unwrap(),
        keyring,
    )
}

/// A manager with `driver` registered, the given secret `stores` and `known_hosts` under `dir`.
pub fn manager_with_stores(
    driver: Arc<dyn Driver>,
    dir: &Path,
    stores: SecretStores,
    tune: impl FnOnce(&mut ManagerConfig),
) -> SessionManager {
    let mut registry = DriverRegistry::new();
    registry.register(driver);
    let mut config = ManagerConfig::with_known_hosts(KnownHostsFiles {
        own: dir.join("known_hosts"),
        user: None,
    });
    config.workers = 2;
    config.connect_timeout = Duration::from_secs(5);
    config.busy_wait = Duration::from_millis(400);
    tune(&mut config);
    SessionManager::new(config, registry, stores).unwrap()
}

/// The mock driver, declaring every TLS mode: the mock's own declaration has none (its
/// connections are not encrypted), and `validate_for_driver` refuses a TLS mode a driver does
/// not declare, so the model's default `verify-full` would not validate.
pub fn mock_driver() -> MockDriver {
    MockDriver::new().with_capabilities(mock_capabilities())
}

/// [`ninesql_core::driver_api::mock::full_capabilities`] with every TLS mode declared.
pub fn mock_capabilities() -> Capabilities {
    let mut caps = ninesql_core::driver_api::mock::full_capabilities();
    caps.tls = [
        api::TlsMode::Disable,
        api::TlsMode::Require,
        api::TlsMode::VerifyCa,
        api::TlsMode::VerifyFull,
    ]
    .into_iter()
    .collect();
    caps
}

pub fn mock_engine() -> EngineId {
    EngineId::new(MOCK_ENGINE.as_str()).unwrap()
}

/// A mock connection without credentials, of type `ty`.
pub fn mock_connection(ty: ConnectionTypeId) -> Connection {
    Connection::new(
        "Mock",
        mock_engine(),
        Endpoint::Tcp {
            host: "mock.example.test".into(),
            port: Some(5000),
            database: Some("app".into()),
        },
        Auth::None,
        ty,
    )
}

/// The "UI executor": a current-thread runtime, distinct from the driver runtime.
pub fn ui<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(f)
}

pub async fn open(
    manager: &SessionManager,
    request: OpenRequest,
) -> Result<OpenOutcome, CoreError> {
    tokio::time::timeout(Duration::from_secs(20), manager.open(request))
        .await
        .expect("open timed out")
        .expect("open task ended without answer")
}

pub async fn open_ok(
    manager: &SessionManager,
    c: Connection,
    types: &ConnectionTypes,
) -> SessionHandle {
    match open(manager, OpenRequest::new(c, types)).await.unwrap() {
        OpenOutcome::Opened(h) => h,
        other => panic!("not opened: {other:?}"),
    }
}

/// Collects every event of an execution up to its terminal one.
pub async fn collect(mut e: Execution) -> Vec<ExecEvent> {
    let mut out = Vec::new();
    loop {
        match tokio::time::timeout(Duration::from_secs(20), e.next()).await {
            Ok(Some(ev)) => {
                let terminal = matches!(
                    ev,
                    ExecEvent::Finished { .. }
                        | ExecEvent::Failed(_)
                        | ExecEvent::NeedsConfirmation(_)
                );
                out.push(ev);
                if terminal {
                    return out;
                }
            }
            Ok(None) => return out,
            Err(_) => panic!("execution timed out; events so far: {out:?}"),
        }
    }
}

pub async fn run(h: &SessionHandle, sql: &str) -> Vec<ExecEvent> {
    collect(h.execute(ExecuteRequest::sql(sql))).await
}

pub fn terminal(events: &[ExecEvent]) -> &ExecEvent {
    events.last().expect("no event")
}
