//! The application's connections: saved connections and types, the core's `SessionManager`,
//! the secret stores and the open sessions, shared by every window (one GPUI entity in a
//! global).
//!
//! I6: nothing here waits on I/O on the foreground thread. Workspace writes and secret-store
//! calls run on the driver runtime's blocking pool (`spawn_blocking`), connects run on the
//! runtime through `SessionManager::open`, and the answers come back as futures that GPUI awaits
//! on its own executor. The only file reads on the foreground thread happen in [`assemble`],
//! before the event loop starts (as for settings.toml).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::{App, AppContext as _, Context, Entity, Global, Task};
use ninesql_core::secrets::{SecretId, SecretMeta, SecretRef, SecretStores, StoreKind};
use ninesql_core::storage::{Paths, ProjectName};
use ninesql_core::{
    Connection, ConnectionId, ConnectionTypes, Connections, CoreEvent, CoreEvents, DriverRegistry,
    ManagerConfig, SessionHandle, SessionManager, Workspace,
};

use super::engines::{self, EngineOffer};
use super::form::BuiltConnection;

/// Everything the hub starts from. Built by [`assemble`] in the application, by hand in tests.
pub struct HubConfig {
    /// Registered drivers.
    pub registry: DriverRegistry,
    /// The session manager (driver runtime); `None` when it could not start.
    pub manager: Option<SessionManager>,
    /// The open project; `None` when it could not be opened (changes then stay in memory).
    pub workspace: Option<Workspace>,
    /// Saved connections.
    pub connections: Connections,
    /// Connection types.
    pub types: ConnectionTypes,
    /// Secret stores.
    ///
    /// INTEGRATION POINT (core, feat/p1-d-app-wiring): `CoreServices::start` assembles the
    /// stores (keyring → vault → ask, `LockHooks`, `DesktopLockWatcher`, idle lock); until it
    /// lands, [`assemble`] registers no store, so every credential is "ask every time" and the
    /// dialog says that no keyring or vault is available. Hook-up: take the stores, workspace,
    /// session manager and registry from `CoreServices`, and `secret_backend()` for the label.
    pub stores: SecretStores,
    /// Problems found while starting: load issues (`LoadIssue`), secret-looking keys removed,
    /// a workspace or runtime that could not start. Shown in the "connection problems" dialog.
    pub problems: Vec<String>,
}

impl HubConfig {
    /// No driver, no runtime, no workspace: what the shell runs with when nothing else was
    /// installed (and the base of most tests).
    #[must_use]
    pub fn empty() -> Self {
        Self {
            registry: DriverRegistry::new(),
            manager: None,
            workspace: None,
            connections: Connections::default(),
            types: ConnectionTypes::default(),
            stores: SecretStores::new(),
            problems: Vec::new(),
        }
    }
}

/// Opens the default project, loads its connections and types and starts the session manager
/// with `registry`. Runs before the event loop (small local reads, as for settings.toml).
/// Failures become problems shown in the UI; NineSQL still starts.
#[must_use]
pub fn assemble(registry: DriverRegistry) -> HubConfig {
    let mut config = HubConfig::empty();
    let paths = match Paths::from_env() {
        Ok(p) => Some(p),
        Err(e) => {
            config.problems.push(format!(
                "No workspace location ({e}): connections are not saved."
            ));
            None
        }
    };
    if let Some(paths) = &paths {
        match Workspace::open(paths.clone(), ProjectName::default_project()) {
            Ok(ws) => {
                match ws.load_connections() {
                    Ok(loaded) => {
                        for issue in &loaded.issues {
                            config
                                .problems
                                .push(format!("Connection \"{}\": {}", issue.name, issue.problem));
                        }
                        for key in &loaded.secret_like_keys {
                            config.problems.push(format!(
                                "connections.json: the key `{key}` looked like a secret and was removed."
                            ));
                        }
                        config.connections = loaded.connections;
                    }
                    Err(e) => config
                        .problems
                        .push(format!("Cannot load the connections: {e}")),
                }
                match ws.load_connection_types() {
                    Ok(t) => config.types = t,
                    Err(e) => config.problems.push(format!(
                        "Cannot load the connection types ({e}): built-in types are used."
                    )),
                }
                config.workspace = Some(ws);
            }
            Err(e) => config.problems.push(format!(
                "Cannot open the workspace ({e}): connections are not saved in this window."
            )),
        }
    }
    let manager_config = match &paths {
        Some(p) => ManagerConfig::new(p),
        None => ManagerConfig::with_known_hosts(ninesql_core::KnownHostsFiles {
            own: std::env::temp_dir().join("ninesql-known_hosts-unsaved"),
            user: None,
        }),
    };
    match SessionManager::new(manager_config, registry.clone(), config.stores.clone()) {
        Ok(m) => config.manager = Some(m),
        Err(e) => config.problems.push(format!(
            "The driver runtime did not start ({e}): connections cannot be opened."
        )),
    }
    config.registry = registry;
    config
}

/// The hub entity, as a GPUI global.
#[derive(Clone)]
pub struct Hub(pub Entity<ConnectionHub>);

impl Global for Hub {}

impl Hub {
    /// The hub of the application, if installed.
    #[must_use]
    pub fn get(cx: &App) -> Option<Entity<ConnectionHub>> {
        cx.try_global::<Self>().map(|h| h.0.clone())
    }
}

/// Installs a hub built from `config` as the application's, starts reading the core's events
/// and replaces any previous hub.
pub fn install(config: HubConfig, cx: &mut App) -> Entity<ConnectionHub> {
    let hub = cx.new(|cx| {
        let mut hub = ConnectionHub::new(config);
        hub.start_event_pump(cx);
        hub
    });
    cx.set_global(Hub(hub.clone()));
    hub
}

/// State of the open sessions of one connection.
#[derive(Debug, Clone, Default)]
pub struct OpenSessions {
    /// Handles, oldest first.
    pub handles: Vec<SessionHandle>,
}

/// Saved connections, types, stores and open sessions.
pub struct ConnectionHub {
    registry: DriverRegistry,
    offers: Vec<EngineOffer>,
    manager: Option<SessionManager>,
    workspace: Option<Workspace>,
    connections: Connections,
    types: ConnectionTypes,
    stores: SecretStores,
    problems: Vec<String>,
    sessions: BTreeMap<ConnectionId, OpenSessions>,
    dropped_seen: u64,
    resyncs: u64,
    saving: bool,
    _events: Option<Task<()>>,
}

impl ConnectionHub {
    fn new(config: HubConfig) -> Self {
        let offers = engines::offers(&config.registry);
        Self {
            registry: config.registry,
            offers,
            manager: config.manager,
            workspace: config.workspace,
            connections: config.connections,
            types: config.types,
            stores: config.stores,
            problems: config.problems,
            sessions: BTreeMap::new(),
            dropped_seen: 0,
            resyncs: 0,
            saving: false,
            _events: None,
        }
    }

    /// Engines the dialog offers (I8).
    #[must_use]
    pub fn offers(&self) -> &[EngineOffer] {
        &self.offers
    }

    /// The offer of an engine.
    #[must_use]
    pub fn offer(&self, engine: &str) -> Option<&EngineOffer> {
        self.offers.iter().find(|o| o.engine == engine)
    }

    /// The driver registry.
    #[must_use]
    pub fn registry(&self) -> &DriverRegistry {
        &self.registry
    }

    /// The session manager, if the runtime started.
    #[must_use]
    pub fn manager(&self) -> Option<&SessionManager> {
        self.manager.as_ref()
    }

    /// Saved connections.
    #[must_use]
    pub fn connections(&self) -> &Connections {
        &self.connections
    }

    /// A saved connection.
    #[must_use]
    pub fn connection(&self, id: ConnectionId) -> Option<&Connection> {
        self.connections.get(id)
    }

    /// Connection types.
    #[must_use]
    pub fn types(&self) -> &ConnectionTypes {
        &self.types
    }

    /// Where a newly saved secret goes; `None` when only "ask every time" is possible.
    #[must_use]
    pub fn store_for_new_secrets(&self) -> Option<StoreKind> {
        self.stores.store_for_new_secrets()
    }

    /// Startup and load problems to show to the user.
    #[must_use]
    pub fn problems(&self) -> &[String] {
        &self.problems
    }

    /// Adds a problem to the list.
    pub fn add_problem(&mut self, problem: impl Into<String>, cx: &mut Context<Self>) {
        self.problems.push(problem.into());
        cx.notify();
    }

    /// Whether a save or delete is running.
    #[must_use]
    pub fn is_saving(&self) -> bool {
        self.saving
    }

    /// Whether changes are written to disk.
    #[must_use]
    pub fn persists(&self) -> bool {
        self.workspace.is_some()
    }

    /// Open sessions of a connection.
    #[must_use]
    pub fn sessions(&self, id: ConnectionId) -> &[SessionHandle] {
        self.sessions.get(&id).map_or(&[], |s| s.handles.as_slice())
    }

    /// Whether a connection has an open session.
    #[must_use]
    pub fn is_open(&self, id: ConnectionId) -> bool {
        !self.sessions(id).is_empty()
    }

    /// How many times the open sessions were resynchronized after dropped core events.
    #[must_use]
    pub fn resyncs(&self) -> u64 {
        self.resyncs
    }

    /// Records a session opened by this UI.
    pub fn add_session(&mut self, handle: SessionHandle, cx: &mut Context<Self>) {
        let entry = self.sessions.entry(handle.info().connection).or_default();
        if !entry.handles.iter().any(|h| h.id() == handle.id()) {
            entry.handles.push(handle);
        }
        cx.notify();
    }

    /// Validates a connection against its driver: the core's `DriverRegistry::validate_connection`
    /// (reserved and off-allowlist properties, auth, `verify-ca` without CA file…). Pure: no I/O.
    ///
    /// # Errors
    /// The core's message, which never contains a value.
    pub fn validate(&self, connection: &Connection) -> Result<(), String> {
        self.registry
            .validate_connection(connection)
            .map_err(|e| e.to_string())
    }

    /// Runs blocking work (workspace files, secret stores) off the foreground thread: on the
    /// driver runtime's blocking pool, or GPUI's background executor without a runtime.
    fn blocking<T: Send + 'static>(
        &self,
        cx: &mut Context<Self>,
        work: impl FnOnce() -> Result<T, String> + Send + 'static,
    ) -> Task<Result<T, String>> {
        match &self.manager {
            Some(m) => {
                let join = m.runtime().handle().spawn_blocking(work);
                cx.background_spawn(async move {
                    join.await
                        .map_err(|_| "the operation stopped unexpectedly".to_owned())?
                })
            }
            None => cx.background_spawn(async move { work() }),
        }
    }

    /// Saves a connection built by the dialog: validated with `validate_connection`, typed
    /// secrets stored in the keyring / vault and the fields pointed at them, `connections.json`
    /// written, then unused saved secrets deleted. Resolves to the connection id.
    pub fn save(
        &mut self,
        built: BuiltConnection,
        cx: &mut Context<Self>,
    ) -> Task<Result<ConnectionId, String>> {
        if let Err(e) = self.validate(&built.connection) {
            return Task::ready(Err(e));
        }
        if self.saving {
            return Task::ready(Err("another change is being saved; try again".to_owned()));
        }
        let store = self.store_for_new_secrets();
        let stores = self.stores.clone();
        let workspace = self.workspace.clone();
        let current = self.connections.clone();
        let BuiltConnection {
            connection,
            to_store,
            to_delete,
        } = built;
        let is_new = current.get(connection.id).is_none();
        self.saving = true;
        cx.notify();
        let work = move || -> Result<(Connections, ConnectionId), String> {
            let mut next = current;
            if is_new && let (Some(ws), Some(kind)) = (&workspace, store) {
                // The core's own create path (stores, points, saves, rolls back on failure).
                let id = ws
                    .create_connection(&mut next, connection, to_store, kind, &stores)
                    .map_err(|e| e.to_string())?;
                return Ok((next, id));
            }
            let mut connection = connection;
            // INTEGRATION POINT (core): there is no `Workspace::update_connection`; editing
            // mirrors `create_connection` here (same secret id scheme `<id>:<slot>`). Hook-up:
            // `create_connection_checked` / `save_connections_checked(&conns, &registry)`.
            let mut stored: Vec<SecretRef> = Vec::new();
            let result = (|| {
                for (slot, secret) in &to_store {
                    let kind = store.ok_or("no keyring or vault is available")?;
                    let id = SecretId::new(format!("{}:{}", connection.id, slot.id_suffix()))
                        .map_err(|e| e.to_string())?;
                    let reference = SecretRef::new(kind, id);
                    let secret_kind = connection
                        .set_secret_ref(*slot, reference.clone())
                        .map_err(|e| e.to_string())?;
                    let meta = SecretMeta {
                        connection_id: connection.id.to_string(),
                        kind: secret_kind,
                        label: format!("NineSQL: {}", connection.name),
                    };
                    stores
                        .save(&reference, &meta, secret)
                        .map_err(|e| e.to_string())?;
                    stored.push(reference);
                }
                let id = connection.id;
                match next.get_mut(id) {
                    Some(slot) => *slot = connection.clone(),
                    None => next.insert(connection.clone()).map_err(|e| e.to_string())?,
                }
                if let Some(ws) = &workspace {
                    ws.save_connections(&next).map_err(|e| e.to_string())?;
                }
                Ok::<_, String>(id)
            })();
            drop(to_store);
            match result {
                Ok(id) => {
                    let still_used: Vec<SecretRef> = connection
                        .secret_refs()
                        .into_iter()
                        .map(|(_, _, r)| r.clone())
                        .collect();
                    for r in to_delete.iter().filter(|r| !still_used.contains(r)) {
                        if let Err(e) = stores.delete(r) {
                            tracing::warn!(reference = %r, error = %e, "could not delete an unused secret");
                        }
                    }
                    Ok((next, id))
                }
                Err(e) => {
                    if is_new {
                        for r in &stored {
                            let _ = stores.delete(r);
                        }
                    }
                    Err(e)
                }
            }
        };
        let task = self.blocking(cx, work);
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |hub, cx| {
                hub.saving = false;
                let out = result.map(|(next, id)| {
                    hub.connections = next;
                    id
                });
                cx.notify();
                out
            })
            .map_err(|_| "the application is closing".to_owned())?
        })
    }

    /// Deletes a connection: its sessions are closed, `connections.json` is written and its
    /// saved secrets are deleted.
    pub fn remove(&mut self, id: ConnectionId, cx: &mut Context<Self>) -> Task<Result<(), String>> {
        if self.saving {
            return Task::ready(Err("another change is being saved; try again".to_owned()));
        }
        if self.connections.get(id).is_none() {
            return Task::ready(Err("this connection no longer exists".to_owned()));
        }
        self.disconnect(id, cx);
        let stores = self.stores.clone();
        let workspace = self.workspace.clone();
        let current = self.connections.clone();
        self.saving = true;
        cx.notify();
        let task = self.blocking(cx, move || {
            let mut next = current;
            match &workspace {
                Some(ws) => {
                    ws.remove_connection(&mut next, id, &stores)
                        .map_err(|e| e.to_string())?;
                }
                None => {
                    if let Some(removed) = next.remove(id) {
                        for (_, _, r) in removed.secret_refs() {
                            let _ = stores.delete(r);
                        }
                    }
                }
            }
            Ok(next)
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |hub, cx| {
                hub.saving = false;
                let out = result.map(|next| hub.connections = next);
                cx.notify();
                out
            })
            .map_err(|_| "the application is closing".to_owned())?
        })
    }

    /// Closes every open session of a connection (on the runtime; the `SessionClosed` events
    /// confirm it).
    pub fn disconnect(&mut self, id: ConnectionId, cx: &mut Context<Self>) {
        if let Some(open) = self.sessions.remove(&id) {
            for handle in open.handles {
                drop(handle.close());
            }
            cx.notify();
        }
    }

    fn start_event_pump(&mut self, cx: &mut Context<Self>) {
        let Some(mut events): Option<CoreEvents> =
            self.manager.as_ref().and_then(SessionManager::take_events)
        else {
            return;
        };
        self._events = Some(cx.spawn(async move |this, cx| {
            while let Some(event) = events.recv().await {
                if this
                    .update(cx, |hub, cx| hub.on_core_event(event, cx))
                    .is_err()
                {
                    break;
                }
            }
        }));
    }

    /// Applies one core event, then resynchronizes if events were dropped meanwhile.
    pub fn on_core_event(&mut self, event: CoreEvent, cx: &mut Context<Self>) {
        match event {
            CoreEvent::SessionOpened { session, .. } => {
                if let Some(handle) = self.manager.as_ref().and_then(|m| m.session(session)) {
                    let entry = self.sessions.entry(handle.info().connection).or_default();
                    if !entry.handles.iter().any(|h| h.id() == session) {
                        entry.handles.push(handle);
                    }
                }
            }
            CoreEvent::SessionClosed {
                session,
                connection,
                reason,
            } => {
                tracing::debug!(session = %session, reason = reason.kind(), "session closed");
                if let Some(open) = self.sessions.get_mut(&connection) {
                    open.handles.retain(|h| h.id() != session);
                    if open.handles.is_empty() {
                        self.sessions.remove(&connection);
                    }
                }
            }
            // INTEGRATION POINT (core, feat/p1-d-app-wiring): `CoreEvent::SecretsLocked
            // { reason }` lands here; hook-up: a status-bar notice "Secrets locked (sleep /
            // screen lock / idle)".
            _ => {}
        }
        self.check_dropped_events();
        cx.notify();
    }

    /// When the core dropped events (its channel was full), rebuilds the list of open sessions
    /// from `SessionManager::sessions_of` (docs/02, P1-C).
    pub fn check_dropped_events(&mut self) {
        let Some(manager) = &self.manager else {
            return;
        };
        let dropped = manager.dropped_events();
        if dropped <= self.dropped_seen {
            return;
        }
        self.dropped_seen = dropped;
        self.resyncs += 1;
        self.sessions = self
            .connections
            .iter()
            .filter_map(|c| {
                let handles = manager.sessions_of(c.id);
                (!handles.is_empty()).then_some((c.id, OpenSessions { handles }))
            })
            .collect();
        tracing::debug!(
            dropped,
            "core events were dropped: open sessions resynchronized"
        );
    }

    /// Closes every session at exit, waiting at most `timeout` (blocking: called once from
    /// the quit path, after the windows are gone).
    pub fn shutdown(&mut self, timeout: Duration) {
        self.sessions.clear();
        if let Some(m) = self.manager.take() {
            m.shutdown(timeout);
        }
    }
}

/// The application's hub, or a fresh empty one for code paths that run without
/// [`install`] (never in the application).
#[must_use]
pub fn hub_or_install_empty(cx: &mut App) -> Entity<ConnectionHub> {
    match Hub::get(cx) {
        Some(h) => h,
        None => install(HubConfig::empty(), cx),
    }
}

/// Shared ownership helper for tests and callers that hold drivers as trait objects.
#[must_use]
pub fn registry_with(drivers: Vec<Arc<dyn ninesql_core::driver_api::Driver>>) -> DriverRegistry {
    let mut r = DriverRegistry::new();
    for d in drivers {
        r.register(d);
    }
    r
}
