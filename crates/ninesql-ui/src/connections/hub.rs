//! The application's connections: saved connections and types, the core's `SessionManager`,
//! the secret stores and the open sessions, shared by every window (one GPUI entity in a
//! global).
//!
//! I6: nothing here waits on I/O on the foreground thread. Workspace writes and secret-store
//! calls run on the driver runtime's blocking pool (`spawn_blocking`), connects run on the
//! runtime through `SessionManager::open`, and the answers come back as futures that GPUI awaits
//! on its own executor. The core itself is started by [`start`]: `CoreConfig::new` (which
//! canonicalizes `$HOME`), `CoreServices::start` and the first load of the connections all run
//! off the UI thread; the hub is installed empty ("starting") and filled when they are done.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::{App, AppContext as _, Context, Entity, EventEmitter, Global, Task};
use ninesql_core::dbeaver::{InstallKind, SearchRoots};
use ninesql_core::secrets::{SecretId, SecretMeta, SecretRef, SecretStores, StoreKind};
use ninesql_core::storage::{Paths, ProjectName};
use ninesql_core::{
    Connection, ConnectionId, ConnectionTypes, Connections, CoreConfig, CoreEvent, CoreEvents,
    CoreServices, DriverRegistry, ImportChoices, ImportError, ImportOutcome, ImportPreview,
    SecretBackend, SecretSlot, SecretsLockReason, SessionHandle, SessionManager, Workspace,
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
    /// Secret stores (from `CoreServices::secret_stores`: keyring when a Secret Service
    /// answers, the vault always, never plaintext).
    pub stores: SecretStores,
    /// Where new secrets go (`CoreServices::secret_backend`); `None`: the first store of
    /// `stores` that takes new secrets (tests without a core).
    pub backend: Option<SecretBackend>,
    /// The started core (vault unlock / create, "lock secrets", lock watcher, shutdown);
    /// `None` in tests that assemble the parts by hand.
    pub core: Option<CoreServices>,
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
            backend: None,
            core: None,
            problems: Vec::new(),
        }
    }

    /// The hub's parts from a started core: its workspace, session manager, registry, stores
    /// and secret backend, plus the saved connections and types. **Blocking** (reads
    /// `connections.json` and `connection-types.json`): call it off the UI thread.
    #[must_use]
    pub fn load_from_core(core: CoreServices) -> Self {
        let mut config = Self::empty();
        let ws = core.workspace().clone();
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
        if let Some(e) = core.vault_error() {
            config.problems.push(format!(
                "The vault cannot be used ({e}): passwords cannot be saved in it."
            ));
        }
        config.registry = core.registry().clone();
        config.manager = Some(core.sessions().clone());
        config.workspace = Some(ws);
        config.stores = core.secret_stores().clone();
        config.backend = Some(core.secret_backend());
        config.core = Some(core);
        config
    }
}

/// Installs the application's hub, empty and "starting", and starts the core off the UI
/// thread: `CoreConfig::new` (blocking: it canonicalizes `$HOME`), `CoreServices::start`
/// (awaited on GPUI's executor; it opens the workspace and assembles the secret stores, the
/// vault, the lock hooks and the desktop lock watcher) and the first load of the connections.
/// The hub is filled when they are done; a failure becomes a connection problem.
pub fn start(registry: DriverRegistry, cx: &mut App) -> Entity<ConnectionHub> {
    let mut config = HubConfig::empty();
    config.registry = registry.clone();
    let hub = install(config, cx);
    hub.update(cx, |h, _| h.starting = true);
    let startup = cx.background_spawn(async move {
        let paths = Paths::from_env()
            .map_err(|e| format!("No workspace location ({e}): connections are not available."))?;
        let config = CoreConfig::new(paths, ProjectName::default_project(), registry);
        let core = CoreServices::start(config)
            .await
            .ok_or_else(|| "The core did not start.".to_owned())?
            .map_err(|e| format!("The core did not start ({e}): connections are not available."))?;
        Ok::<_, String>(HubConfig::load_from_core(core))
    });
    let weak = hub.downgrade();
    cx.spawn(async move |cx| {
        let result = startup.await;
        let _ = weak.update(cx, |h, cx| h.attach(result, cx));
    })
    .detach();
    hub
}

/// A fresh reference for a secret about to be stored: `<connection>:<slot>:<random>`. Never
/// reuses the id of another slot or of an earlier value, so writing it can never overwrite a
/// secret that some field (of this or another connection) still points at.
fn fresh_reference(
    kind: StoreKind,
    connection: ConnectionId,
    slot: SecretSlot,
) -> Result<SecretRef, String> {
    let id = SecretId::new(format!(
        "{connection}:{}:{}",
        slot.id_suffix(),
        uuid::Uuid::new_v4().simple()
    ))
    .map_err(|e| e.to_string())?;
    Ok(SecretRef::new(kind, id))
}

/// Whether `id` is referenced by any saved connection or by `connection` (the version being
/// saved).
fn is_referenced(id: &SecretId, connections: &Connections, connection: &Connection) -> bool {
    connections
        .iter()
        .chain(std::iter::once(connection))
        .any(|c| c.secret_refs().iter().any(|(_, _, r)| &r.id == id))
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

/// A DBeaver workspace found in the known locations (first-run import offer, P2-B). Only the
/// location and the number of projects: no DBeaver file content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedDbeaver {
    /// How DBeaver was installed.
    pub kind: InstallKind,
    /// Workspace folder.
    pub path: std::path::PathBuf,
    /// Projects with a data-sources file.
    pub projects: usize,
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
    backend: Option<SecretBackend>,
    core: Option<CoreServices>,
    starting: bool,
    secrets_locked: Option<SecretsLockReason>,
    problems: Vec<String>,
    sessions: BTreeMap<ConnectionId, OpenSessions>,
    dropped_seen: u64,
    resyncs: u64,
    saving: bool,
    importing: bool,
    detected: Option<Vec<DetectedDbeaver>>,
    local_execution: Option<ninesql_core::ExecutionService>,
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
            backend: config.backend,
            core: config.core,
            starting: false,
            secrets_locked: None,
            problems: config.problems,
            sessions: BTreeMap::new(),
            dropped_seen: 0,
            resyncs: 0,
            saving: false,
            importing: false,
            detected: None,
            local_execution: None,
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
        match self.backend {
            Some(b) => b.store(),
            None => self.stores.store_for_new_secrets(),
        }
    }

    /// The started core, once [`start`] finished (vault, "lock secrets", lock watcher).
    #[must_use]
    pub fn core(&self) -> Option<&CoreServices> {
        self.core.as_ref()
    }

    /// Whether the core is still starting.
    #[must_use]
    pub fn is_starting(&self) -> bool {
        self.starting
    }

    /// Why secrets were last locked, until the UI unlocks the vault again.
    #[must_use]
    pub fn secrets_locked(&self) -> Option<SecretsLockReason> {
        self.secrets_locked
    }

    /// The vault was unlocked (or created) from the UI.
    pub fn mark_unlocked(&mut self, cx: &mut Context<Self>) {
        self.secrets_locked = None;
        cx.notify();
    }

    /// Fills a hub installed by [`start`] with the started core, or records why it failed.
    pub fn attach(&mut self, result: Result<HubConfig, String>, cx: &mut Context<Self>) {
        self.starting = false;
        match result {
            Ok(config) => {
                let mut problems = std::mem::take(&mut self.problems);
                let mut next = Self::new(config);
                next.problems.append(&mut problems);
                *self = next;
                self.start_event_pump(cx);
                // First-run import offer (P2-B): only looked for when there is nothing saved.
                if self.connections.is_empty()
                    && let Some(roots) = SearchRoots::from_env()
                {
                    self.detect_dbeaver(roots, cx);
                }
            }
            Err(e) => self.problems.push(e),
        }
        cx.notify();
    }

    /// Startup and load problems to show to the user, plus the state of the desktop lock
    /// watcher when it does not run (read live: it may lose the system bus later).
    #[must_use]
    pub fn problems(&self) -> Vec<String> {
        let mut out = self.problems.clone();
        // Saving writes every connection through `save_connections_checked`, which refuses
        // the whole file when one of them is invalid: name those, with the reason.
        for c in self.connections.iter() {
            if let Err(e) = self.check_saved(c) {
                out.push(format!(
                    "Connection \"{}\" is refused by its driver ({e}): no connection can be saved until it is fixed or deleted.",
                    c.name
                ));
            }
        }
        if let Some(core) = &self.core {
            let status = core.lock_watcher_status();
            if !status.is_running {
                out.push(match (status.built, &status.error) {
                    (false, _) => "Sleep and screen lock are not watched in this build: the vault does not lock itself when the computer sleeps or the screen locks (it still locks after being idle). Use \"Lock secrets now\" before leaving the computer.".to_owned(),
                    (true, Some(e)) => format!("Sleep and screen lock are not watched ({e}): use \"Lock secrets now\" before leaving the computer."),
                    (true, None) => "Sleep and screen lock are not watched: use \"Lock secrets now\" before leaving the computer.".to_owned(),
                });
            }
        }
        out
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

    /// Every open session, by connection name (the editor's connection picker).
    #[must_use]
    pub fn open_sessions(&self) -> Vec<SessionHandle> {
        let mut out: Vec<SessionHandle> = self
            .sessions
            .values()
            .flat_map(|s| s.handles.iter().cloned())
            .filter(|h| !h.is_closed())
            .collect();
        out.sort_by(|a, b| a.info().connection_name.cmp(&b.info().connection_name));
        out
    }

    /// The execution service (P3-C): the core's, or one over the session manager when the
    /// hub was assembled without a core (tests). `None` when the driver runtime did not start.
    #[must_use]
    pub fn execution(&mut self) -> Option<ninesql_core::ExecutionService> {
        if let Some(core) = &self.core {
            return Some(core.execution().clone());
        }
        if self.local_execution.is_none()
            && let Some(manager) = &self.manager
        {
            self.local_execution = Some(ninesql_core::ExecutionService::new(
                manager.clone(),
                None,
                ninesql_core::preferences::Preferences::default(),
            ));
        }
        self.local_execution.clone()
    }

    /// The preferences in effect (`[execution]`, `[history]`, `[confirmations]` of
    /// settings.toml), defaults without a core.
    #[must_use]
    pub fn preferences(&self) -> ninesql_core::preferences::Preferences {
        match (&self.core, &self.local_execution) {
            (Some(core), _) => core.preferences(),
            (None, Some(exec)) => exec.preferences(),
            (None, None) => ninesql_core::preferences::Preferences::default(),
        }
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

    /// The rule of `Workspace::save_connections_checked` for one saved connection: driver
    /// validation when enabled, the model rules when disabled (no driver, imported for later).
    fn check_saved(&self, c: &Connection) -> Result<(), String> {
        if c.enabled {
            self.registry
                .validate_connection(c)
                .map_err(|e| e.to_string())
        } else {
            c.validate().map_err(|e| e.to_string())
        }
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

    /// Saves a connection built by the dialog: validated with `validate_connection`; each typed
    /// secret written to the keyring / vault under a fresh id and its field pointed at it;
    /// `connections.json` written; only then the hub's list is replaced and the secrets that
    /// no connection references any more are deleted. If anything fails before the file is
    /// written, the secrets just stored are deleted again and nothing else changes (the saved
    /// file still points at the old ones). Resolves to the connection id.
    pub fn save(
        &mut self,
        built: BuiltConnection,
        cx: &mut Context<Self>,
    ) -> Task<Result<ConnectionId, String>> {
        // The rule of `save_connections_checked`: the driver for an enabled connection, the
        // model rules for a disabled one (an imported connection with no driver can be edited).
        if let Err(e) = self.check_saved(&built.connection) {
            return Task::ready(Err(e));
        }
        if self.saving || self.importing {
            return Task::ready(Err("another change is being saved; try again".to_owned()));
        }
        if self.starting {
            return Task::ready(Err(
                "NineSQL is still starting; try again in a moment".to_owned()
            ));
        }
        let store = self.store_for_new_secrets();
        let registry = self.registry.clone();
        let stores = self.stores.clone();
        let workspace = self.workspace.clone();
        let current = self.connections.clone();
        let BuiltConnection {
            mut connection,
            to_store,
            to_delete,
        } = built;
        // The UI never sets the imported init SQL marker (only the import does): an edit keeps
        // it only when the hub's current copy still has it, so a form opened before the user
        // confirmed the init SQL does not bring it back.
        connection.init_sql_needs_confirmation =
            connection.init_sql_needs_confirmation && self.init_sql_marker(connection.id);
        self.saving = true;
        cx.notify();
        let work = move || -> Result<(Connections, ConnectionId), String> {
            let mut next = current;
            let mut connection = connection;
            // One path for create and edit (core has no `update_connection`): fresh-id secrets
            // first, then the file through `save_connections_checked`, which runs
            // `DriverRegistry::validate_connection` on every connection before writing.
            let mut stored: Vec<SecretRef> = Vec::new();
            let result = (|| {
                for (slot, secret) in &to_store {
                    let kind = store.ok_or("no keyring or vault is available")?;
                    let reference = fresh_reference(kind, connection.id, *slot)?;
                    if is_referenced(&reference.id, &next, &connection) {
                        return Err(
                            "a saved secret with this id is still in use; nothing was written"
                                .to_owned(),
                        );
                    }
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
                    ws.save_connections_checked(&next, &registry)
                        .map_err(|e| e.to_string())?;
                }
                Ok::<_, String>(id)
            })();
            drop(to_store);
            match result {
                Ok(id) => {
                    for r in &to_delete {
                        if is_referenced(&r.id, &next, &connection) {
                            continue;
                        }
                        if let Err(e) = stores.delete(r) {
                            tracing::warn!(reference = %r, error = %e, "could not delete an unused secret");
                        }
                    }
                    Ok((next, id))
                }
                Err(e) => {
                    // Fresh ids: nothing on disk points at them.
                    for r in &stored {
                        if let Err(del) = stores.delete(r) {
                            tracing::warn!(reference = %r, error = %del, "could not roll back a stored secret");
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
        if self.saving || self.importing {
            return Task::ready(Err("another change is being saved; try again".to_owned()));
        }
        if self.starting {
            return Task::ready(Err(
                "NineSQL is still starting; try again in a moment".to_owned()
            ));
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

    /// Whether a DBeaver import is being applied (saves and deletes wait for it).
    #[must_use]
    pub fn is_importing(&self) -> bool {
        self.importing
    }

    /// DBeaver workspaces found by [`Self::detect_dbeaver`]; `None` until it answered.
    #[must_use]
    pub fn detected_dbeaver(&self) -> Option<&[DetectedDbeaver]> {
        self.detected.as_deref()
    }

    /// Sets what the DBeaver detection found (tests, and the answer of [`Self::detect_dbeaver`]).
    pub fn set_detected_dbeaver(&mut self, found: Vec<DetectedDbeaver>, cx: &mut Context<Self>) {
        self.detected = Some(found);
        cx.notify();
    }

    /// Looks for DBeaver workspaces in the known locations under `roots` (native, Flatpak,
    /// Snap; ADR-0006 §2), off the UI thread (I6). Only lists folders: nothing is decrypted and
    /// nothing is written (I4). The preview reads the workspace through the core.
    pub fn detect_dbeaver(&mut self, roots: SearchRoots, cx: &mut Context<Self>) {
        let task = self.blocking(cx, move || {
            Ok(ninesql_core::dbeaver::detect_workspaces(&roots)
                .into_iter()
                .map(|w| DetectedDbeaver {
                    kind: w.kind,
                    path: w.path,
                    projects: w.projects.len(),
                })
                .collect::<Vec<_>>())
        });
        cx.spawn(async move |this, cx| {
            let found = task.await.unwrap_or_default();
            let _ = this.update(cx, |hub, cx| hub.set_detected_dbeaver(found, cx));
        })
        .detach();
    }

    /// Applies a DBeaver import preview through `CoreServices::apply_import` (blocking pool,
    /// I6). Saves and deletes are refused while it runs; on success the hub's connections are
    /// replaced by `ImportOutcome::connections` (the list as written) and the connection types
    /// are read again, so the hub never keeps a stale list that a later save would write back
    /// (P2-C review, L-d). The returned outcome's `connections` is empty (moved into the hub).
    ///
    /// The work runs in a detached task owned by the hub, not by the caller: dropping the
    /// returned task only drops the answer. The hub still takes the list as written and resets
    /// its "importing" state, so saves are never refused for the rest of the session (P2-D
    /// review, L2b).
    pub fn apply_import(
        &mut self,
        preview: ImportPreview,
        choices: ImportChoices,
        cx: &mut Context<Self>,
    ) -> Task<Result<ImportOutcome, ImportError>> {
        if self.saving || self.importing {
            return Task::ready(Err(ImportError::Runtime(
                "another change is being saved; try again".to_owned(),
            )));
        }
        let Some(core) = self.core.clone() else {
            return Task::ready(Err(ImportError::Runtime(
                "NineSQL is still starting, or its core did not start".to_owned(),
            )));
        };
        let reply = core.apply_import(preview, choices);
        self.importing = true;
        cx.notify();
        let (tx, rx) = local_oneshot::<Result<ImportOutcome, ImportError>>();
        cx.spawn(async move |this, cx| {
            let result = reply
                .await
                .unwrap_or_else(|| Err(ImportError::Runtime("the import stopped".to_owned())));
            let types = match &result {
                Ok(_) => {
                    let ws = core.workspace().clone();
                    let task = this
                        .update(cx, |hub, cx| {
                            hub.blocking(cx, move || {
                                ws.load_connection_types().map_err(|e| e.to_string())
                            })
                        })
                        .ok();
                    match task {
                        Some(t) => Some(t.await),
                        None => None,
                    }
                }
                Err(_) => None,
            };
            let out = this
                .update(cx, |hub, cx| {
                    hub.importing = false;
                    let out = result.map(|mut outcome| {
                        hub.connections = std::mem::take(&mut outcome.connections);
                        match types {
                            Some(Ok(t)) => hub.types = t,
                            Some(Err(e)) => hub.problems.push(format!(
                                "Cannot read the connection types after the import ({e}): restart NineSQL to see them."
                            )),
                            None => {}
                        }
                        outcome
                    });
                    cx.notify();
                    out
                })
                .unwrap_or_else(|_| {
                    Err(ImportError::Runtime("the application is closing".to_owned()))
                });
            tx.send(out);
        })
        .detach();
        cx.spawn(async move |_, _| {
            rx.await
                .unwrap_or_else(|| Err(ImportError::Runtime("the import stopped".to_owned())))
        })
    }

    /// Enables a disabled connection through `Workspace::enable_connection` (the core clears
    /// the disabled reason only when its cause is gone; a `Route` reason also needs
    /// `route_fixed`, passed only after the user confirmed the route was set up again). On
    /// success the hub's list is replaced by the one written. The error says why it was
    /// refused.
    pub fn enable(
        &mut self,
        id: ConnectionId,
        route_fixed: bool,
        cx: &mut Context<Self>,
    ) -> Task<Result<(), String>> {
        if self.saving || self.importing {
            return Task::ready(Err("another change is being saved; try again".to_owned()));
        }
        if self.starting {
            return Task::ready(Err(
                "NineSQL is still starting; try again in a moment".to_owned()
            ));
        }
        if self.connections.get(id).is_none() {
            return Task::ready(Err("this connection no longer exists".to_owned()));
        }
        let registry = self.registry.clone();
        let workspace = self.workspace.clone();
        let current = self.connections.clone();
        self.saving = true;
        cx.notify();
        let task = self.blocking(cx, move || {
            let mut next = current;
            match &workspace {
                Some(ws) => ws
                    .enable_connection(&mut next, id, route_fixed, &registry)
                    .map_err(|e| e.to_string())?,
                None => next
                    .get_mut(id)
                    .ok_or_else(|| "this connection no longer exists".to_owned())?
                    .try_enable(&registry, route_fixed)
                    .map_err(|e| e.to_string())?,
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
        let events = match &self.core {
            Some(core) => core.take_events(),
            None => self.manager.as_ref().and_then(SessionManager::take_events),
        };
        let Some(mut events): Option<CoreEvents> = events else {
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
            CoreEvent::SecretsLocked { reason } => {
                self.secrets_locked = Some(reason);
                cx.emit(HubEvent::SecretsLocked(reason));
            }
            CoreEvent::TransactionChanged { session, state, .. } => {
                cx.emit(HubEvent::TransactionChanged { session, state });
            }
            CoreEvent::ImportedInitSqlConfirmed {
                connection,
                statements,
                cleared,
            } => {
                // The hub's copy follows the core only for the statements the user confirmed,
                // and only when `connections.json` holds them without the marker (`None`: the
                // core has no workspace, the statements alone decide). A copy with other init
                // SQL (re-imported meanwhile) or a failed write keeps the marker, so the next
                // connect asks again; a later save then does not write a cleared marker.
                // A save that started before the event may still write its snapshot back
                // with the marker: that only asks once more (fail-closed).
                if cleared != Some(false)
                    && let Some(c) = self.connections.get_mut(connection)
                    && c.init_sql == statements
                {
                    c.init_sql_needs_confirmation = false;
                }
            }
            _ => {}
        }
        self.check_dropped_events();
        cx.notify();
    }

    /// Whether the hub's copy of connection `id` still carries the imported init SQL marker
    /// (`Connection::init_sql_needs_confirmation`). `false` for an unknown connection.
    #[must_use]
    pub fn init_sql_marker(&self, id: ConnectionId) -> bool {
        self.connections
            .get(id)
            .is_some_and(|c| c.init_sql_needs_confirmation)
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
        let manager = self.manager.take();
        match self.core.take() {
            Some(core) => {
                drop(manager);
                core.shutdown(timeout);
            }
            None => {
                if let Some(m) = manager {
                    m.shutdown(timeout);
                }
            }
        }
    }
}

/// Events of the hub, for the windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HubEvent {
    /// The core locked the secrets (vault locked, "ask every time" answers forgotten).
    SecretsLocked(SecretsLockReason),
    /// The transaction state of a session changed (`CoreEvent::TransactionChanged`): the
    /// editors on that session update their transaction indicator (F-SQL-07).
    TransactionChanged {
        /// Session.
        session: ninesql_core::SessionId,
        /// The new state.
        state: ninesql_core::TransactionState,
    },
}

impl EventEmitter<HubEvent> for ConnectionHub {}

/// What the status notice says about a lock.
#[must_use]
pub fn lock_reason_text(reason: SecretsLockReason) -> &'static str {
    match reason {
        SecretsLockReason::Sleep => "the computer went to sleep",
        SecretsLockReason::SessionLock => "the screen was locked",
        SecretsLockReason::Idle => "the vault was not used for a while",
        _ => "you asked for it",
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

/// A single-threaded one-shot channel: the detached import task answers the caller's task
/// through it (both run on the foreground executor). The receiver yields `None` when the
/// sender is dropped without answering.
fn local_oneshot<T>() -> (LocalSender<T>, LocalReceiver<T>) {
    let slot = std::rc::Rc::new(std::cell::RefCell::new(LocalSlot {
        value: None,
        closed: false,
        waker: None,
    }));
    (LocalSender(slot.clone()), LocalReceiver(slot))
}

struct LocalSlot<T> {
    value: Option<T>,
    closed: bool,
    waker: Option<std::task::Waker>,
}

struct LocalSender<T>(std::rc::Rc<std::cell::RefCell<LocalSlot<T>>>);

impl<T> LocalSender<T> {
    fn send(self, value: T) {
        self.0.borrow_mut().value = Some(value);
        // `Drop` marks the slot closed and wakes the receiver.
    }
}

impl<T> Drop for LocalSender<T> {
    fn drop(&mut self) {
        let waker = {
            let mut slot = self.0.borrow_mut();
            slot.closed = true;
            slot.waker.take()
        };
        if let Some(w) = waker {
            w.wake();
        }
    }
}

struct LocalReceiver<T>(std::rc::Rc<std::cell::RefCell<LocalSlot<T>>>);

impl<T> std::future::Future for LocalReceiver<T> {
    type Output = Option<T>;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<T>> {
        let mut slot = self.0.borrow_mut();
        if let Some(v) = slot.value.take() {
            return std::task::Poll::Ready(Some(v));
        }
        if slot.closed {
            return std::task::Poll::Ready(None);
        }
        slot.waker = Some(cx.waker().clone());
        std::task::Poll::Pending
    }
}
