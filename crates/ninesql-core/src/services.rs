//! Startup assembly of the core (ROADMAP: wave 2 integration review L5, P1-C prerequisites).
//!
//! [`CoreServices::start`] does, off the UI thread (I6), everything the application needs before
//! its first connection:
//! 1. opens the project's [`Workspace`] (directories, workspace lock, `project.json`);
//! 2. selects the secret backend, **keyring → vault → ask every time**, never plaintext
//!    (ADR-0005, I1): the Secret Service when one answers (feature `secret-service`), the
//!    encrypted [`Vault`] always registered (opened locked) so references saved in it keep
//!    resolving, and the "ask every time" [`SessionSecretCache`];
//! 3. registers the vault and the cache in one [`LockHooks`], with a hook that reports
//!    [`CoreEvent::SecretsLocked`];
//! 4. starts the [`SessionManager`] (driver runtime) with the drivers the application
//!    registered in its [`DriverRegistry`];
//! 5. starts `DesktopLockWatcher` (feature `desktop-lock`, on by default): sleep and screen lock
//!    lock the vault and clear the cache; a watcher that loses the system bus or logind locks
//!    once and recovers on its own, and [`CoreServices::lock_watcher_status`] says so live;
//! 6. runs the idle timer on the driver runtime: [`Vault::lock_if_idle`] and
//!    [`SessionSecretCache::purge_expired`] every [`CoreConfig::lock_check_interval`];
//! 7. reads the execution preferences from `settings.toml` ([`crate::preferences`]), opens the
//!    query history (`history.sqlite`, [`crate::history`]; a history that cannot be opened is
//!    reported by [`CoreServices::history_error`], not an error) and builds the
//!    [`ExecutionService`] (P3-C);
//! 8. builds the [`MetadataService`] (P4-B) with the project's navigator filters
//!    (`navigator.json`).
//!
//! The UI holds the returned [`CoreServices`] (cheap to clone) and calls:
//! - [`CoreServices::sessions`] / [`CoreServices::take_events`] to open sessions and read events;
//! - [`CoreServices::validate_connection`] and the `*_checked` save methods of [`Workspace`];
//! - [`CoreServices::unlock_vault`] / [`CoreServices::create_vault`] when an open answers
//!   [`OpenOutcome::NeedsVaultUnlock`](crate::OpenOutcome::NeedsVaultUnlock), then opens again;
//! - [`CoreServices::lock_now`] for a "lock secrets" action;
//! - [`CoreServices::preview_import`] / [`CoreServices::apply_import`] for the DBeaver import
//!   (P2-B / P2-C, [`crate::import`]); [`CoreServices::export_dbeaver`] for the export to a new
//!   DBeaver file (P5-D, [`crate::dbeaver_export`]);
//! - [`CoreServices::edit`] for row editing, filtered table queries and foreign-key navigation
//!   (P5-A core, [`crate::edit`]);
//! - [`CoreServices::lock_watcher_status`] to say whether sleep / screen lock are watched;
//! - [`CoreServices::execution`] to run statements and scripts, read the history and the
//!   preferences; [`CoreServices::save_preferences`] to change them (P3-C);
//! - [`CoreServices::metadata`] for the navigator tree, DDL, filters, quick search and the
//!   editor's completion data (P4-B).
//!
//! Every call that may block (store access, Argon2id, file I/O) runs on the runtime's blocking
//! pool and answers through a [`Reply`]; nothing here blocks the caller, except
//! [`CoreServices::start_blocking`] and [`CoreServices::shutdown`], meant for a background
//! thread and for exit.

use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

#[cfg(feature = "desktop-lock")]
use ninesql_secrets::WatcherState;
use ninesql_secrets::{
    KdfParams, LockHook, LockHooks, LockTrigger, SecretServiceAvailability, SecretStores,
    SecretString, SessionSecretCache, StoreKind, Vault, VaultStatus, VaultStore,
};
use ninesql_storage::{Paths, ProjectName};
use tokio::task::JoinHandle;

use crate::connection::{Connection, ConnectionId};
use crate::dbeaver_export::{DbeaverExportError, DbeaverExportOutcome};
use crate::edit::EditService;
use crate::error::{CoreError, WorkspaceError};
use crate::execution::ExecutionService;
use crate::history::History;
use crate::import::{
    ImportCancel, ImportChoices, ImportError, ImportOutcome, ImportPreview, ImportSource, Importer,
};
use crate::metadata::{MetadataConfig, MetadataService};
use crate::preferences::{self, PreferenceChange, PreferenceIssue, Preferences, PreferencesError};
use crate::registry::DriverRegistry;
use crate::session::{
    CoreEvent, CoreEvents, EventSink, ManagerConfig, Reply, SecretsLockReason, SessionManager,
};
use crate::transfer::TransferService;
use crate::workspace::Workspace;

/// Default idle time after which the vault locks itself.
pub const DEFAULT_VAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// Default period of the idle timer (vault idle lock, expiry of cached prompts).
pub const DEFAULT_LOCK_CHECK_INTERVAL: Duration = Duration::from_secs(30);

/// Where newly saved secrets go (ADR-0005 §1–§3). There is no plaintext option (I1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SecretBackend {
    /// The desktop keyring (Secret Service).
    Keyring,
    /// The encrypted vault (created with a master password on first use).
    Vault,
    /// Neither: passwords cannot be saved; the dialog offers "ask every time" only.
    AskEveryTime,
}

impl SecretBackend {
    /// The store new secrets are saved to; `None` for [`SecretBackend::AskEveryTime`].
    #[must_use]
    pub const fn store(self) -> Option<StoreKind> {
        match self {
            Self::Keyring => Some(StoreKind::Keyring),
            Self::Vault => Some(StoreKind::Vault),
            Self::AskEveryTime => None,
        }
    }
}

/// Settings of [`CoreServices::start`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct CoreConfig {
    /// NineSQL directories.
    pub paths: Paths,
    /// Project to open.
    pub project: ProjectName,
    /// The drivers the application ships (registered by the application, docs/02).
    pub registry: DriverRegistry,
    /// Session manager settings. Default: [`ManagerConfig::new`] for `paths`.
    pub manager: ManagerConfig,
    /// Probe the Secret Service and use it for new secrets when it answers (only with the
    /// `secret-service` feature). Default `true`.
    pub use_secret_service: bool,
    /// Start the logind watcher (only with the `desktop-lock` feature). Default `true`.
    pub watch_desktop_lock: bool,
    /// Idle time after which the vault locks itself; `None`: never. Default
    /// [`DEFAULT_VAULT_IDLE_TIMEOUT`].
    pub vault_idle_timeout: Option<Duration>,
    /// Period of the idle timer. Default [`DEFAULT_LOCK_CHECK_INTERVAL`]; at least 100 ms.
    pub lock_check_interval: Duration,
    /// Open the query history (`history.sqlite`). Default `true`.
    pub open_history: bool,
}

impl CoreConfig {
    /// Defaults for `project` under `paths`, with the drivers of `registry`. **Blocking**:
    /// [`ManagerConfig::new`] canonicalizes `$HOME` (DBeaver roots); build it off the UI thread,
    /// for example in the closure that then calls [`CoreServices::start_blocking`].
    #[must_use]
    pub fn new(paths: Paths, project: ProjectName, registry: DriverRegistry) -> Self {
        Self {
            manager: ManagerConfig::new(&paths),
            paths,
            project,
            registry,
            use_secret_service: true,
            watch_desktop_lock: true,
            vault_idle_timeout: Some(DEFAULT_VAULT_IDLE_TIMEOUT),
            lock_check_interval: DEFAULT_LOCK_CHECK_INTERVAL,
            open_history: true,
        }
    }
}

/// Why the core could not start.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StartupError {
    /// The workspace could not be opened (another NineSQL holds it, permissions, I/O).
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
    /// The driver runtime could not start.
    #[error(transparent)]
    Core(#[from] CoreError),
    /// A test-build guard refused the start (feature `e2e-scenarios`, ADR-0029 §2.5): guards
    /// not installed, a build with `secret-service`, NineSQL directories outside the run
    /// directory, or no vault.
    #[cfg(feature = "e2e-scenarios")]
    #[error(transparent)]
    TestBuild(#[from] crate::e2e::GuardError),
}

/// State of the sleep / screen-lock watcher, for the UI (a status line, the settings page).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct LockWatcherStatus {
    /// This build has the watcher (feature `desktop-lock`).
    pub built: bool,
    /// The watcher listens now. `false` when it was not started or could not start (`error`),
    /// while it has lost the system bus or logind (it then locked once and reconnects or
    /// waits for logind on its own: read it again, it turns `true` when the watcher is back),
    /// and while the session's screen lock, watched before, is not any more (only sleep is).
    pub is_running: bool,
    /// A logind sleep delay inhibitor is held: the suspend waits until the secrets are locked.
    pub holds_sleep_delay: bool,
    /// The screen lock of the current session is watched (not only sleep).
    pub watches_session_lock: bool,
    /// Why it is not running, without any secret (`None` once it is back).
    pub error: Option<String>,
}

/// Why a started watcher does not watch (fully) now, without any secret (`None` while it
/// watches). The watcher reconnects and waits for logind on its own, so the problem clears by
/// itself. `session_lock_lost`: the session's screen lock was watched before and is not any
/// more (security review M1), reported even while sleep is watched.
#[cfg(feature = "desktop-lock")]
fn watcher_state_problem(state: WatcherState, session_lock_lost: bool) -> Option<String> {
    match state {
        WatcherState::Watching if session_lock_lost => Some(
            "the screen lock is no longer watched (the session was not found again); only sleep locks the secrets"
                .to_owned(),
        ),
        WatcherState::Watching => None,
        WatcherState::Reconnecting => {
            Some("the system bus connection was lost; secrets were locked; reconnecting".to_owned())
        }
        WatcherState::LogindAbsent => {
            Some("logind is not on the system bus; waiting for it to return".to_owned())
        }
        _ => Some("the watcher stopped".to_owned()),
    }
}

/// The vault open in this session, or why there is none.
enum VaultSlot {
    Open(Arc<Vault>),
    Unavailable(String),
}

struct Inner {
    workspace: Workspace,
    manager: SessionManager,
    stores: SecretStores,
    backend: SecretBackend,
    keyring: SecretServiceAvailability,
    vault: VaultSlot,
    cache: Arc<SessionSecretCache>,
    hooks: Arc<LockHooks>,
    #[cfg(feature = "desktop-lock")]
    watcher: Mutex<Option<ninesql_secrets::DesktopLockWatcher>>,
    watcher_error: Option<String>,
    timer: Mutex<Option<JoinHandle<()>>>,
    execution: ExecutionService,
    metadata: MetadataService,
    transfer: TransferService,
    edit: EditService,
    navigator_issues: Vec<String>,
    history_error: Option<String>,
    preference_issues: Vec<PreferenceIssue>,
    paths: Paths,
}

impl Drop for Inner {
    fn drop(&mut self) {
        if let Some(t) = self
            .timer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            t.abort();
        }
        // Secrets leave memory with the services, whatever else still holds the vault.
        self.hooks.lock_all(LockTrigger::Requested);
    }
}

/// The started core: workspace, secret stores, session manager, lock wiring. Cheap to clone;
/// the last clone dropped stops the timer and the watcher and locks the secrets (call
/// [`CoreServices::shutdown`] at exit to also close the sessions cleanly).
#[derive(Clone)]
pub struct CoreServices {
    inner: Arc<Inner>,
}

impl fmt::Debug for CoreServices {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CoreServices")
            .field("project", self.inner.workspace.project())
            .field("backend", &self.inner.backend)
            .field("stores", &self.inner.stores)
            .field("vault", &matches!(self.inner.vault, VaultSlot::Open(_)))
            .finish_non_exhaustive()
    }
}

/// After an unlock or a create that succeeded: when a lock started since `generation` was
/// captured (at the UI's request), it may have run before the vault was unlocked and found
/// nothing to lock. The vault is then locked again and the caller gets
/// [`VaultError::Locked`](ninesql_secrets::VaultError::Locked) (review P1-D wiring, L2).
fn relock_if_locked_since(
    vault: &Vault,
    hooks: &LockHooks,
    generation: u64,
) -> Result<(), CoreError> {
    if hooks.generation() == generation {
        return Ok(());
    }
    vault.lock();
    tracing::info!("secrets were locked while the vault was being unlocked: locked again");
    Err(CoreError::Vault(ninesql_secrets::VaultError::Locked))
}

/// Reports locks triggered through [`LockHooks`] (watcher, [`CoreServices::lock_now`]).
struct EventHook(EventSink);

impl LockHook for EventHook {
    fn lock_now(&self, trigger: LockTrigger) {
        let reason = match trigger {
            LockTrigger::Sleep => SecretsLockReason::Sleep,
            LockTrigger::SessionLock => SecretsLockReason::SessionLock,
            _ => SecretsLockReason::Requested,
        };
        self.0.emit(CoreEvent::SecretsLocked { reason });
    }
}

impl CoreServices {
    /// Starts the core on a background thread: never blocks the caller (I6). Await the
    /// [`Reply`] on any executor or poll it with [`Reply::try_take`]. Startup takes up to a few
    /// seconds when a Secret Service or the system bus is slow to answer (each bounded to 5 s).
    #[must_use]
    pub fn start(config: CoreConfig) -> Reply<Result<Self, StartupError>> {
        let (tx, reply) = Reply::channel();
        let spawned = std::thread::Builder::new()
            .name("ninesql-core-startup".into())
            .spawn(move || {
                let _ = tx.send(Self::start_blocking(config));
            });
        match spawned {
            Ok(_) => reply,
            Err(e) => Reply::ready(Err(StartupError::Core(CoreError::Runtime(format!(
                "cannot start the startup thread: {e}"
            ))))),
        }
    }

    /// [`Self::start`] on the calling thread. **Blocking** (file I/O, D-Bus probes of at most
    /// 5 s each): call it from a background thread or `main` setup, never from the UI thread.
    ///
    /// # Errors
    /// [`StartupError::Workspace`] when the workspace cannot be opened,
    /// [`StartupError::Core`] when the driver runtime cannot start. In a test build (feature
    /// `e2e-scenarios`), `StartupError::TestBuild` unless [`crate::e2e::install`] was called,
    /// the NineSQL directories and `known_hosts` files lie inside its run directory and the
    /// vault opens; the Secret Service is never probed there. A missing keyring, a vault
    /// that cannot be opened or a watcher that cannot start are not errors: they are reported
    /// by [`Self::secret_backend`], [`Self::vault_error`] and [`Self::lock_watcher_status`].
    pub fn start_blocking(config: CoreConfig) -> Result<Self, StartupError> {
        let CoreConfig {
            paths,
            project,
            registry,
            manager: manager_config,
            use_secret_service,
            watch_desktop_lock,
            vault_idle_timeout,
            lock_check_interval,
            open_history,
        } = config;
        // Test build (ADR-0029 §2.5): guards installed, everything inside the run directory,
        // the keyring never probed nor used.
        #[cfg(feature = "e2e-scenarios")]
        let use_secret_service = {
            crate::e2e::refuse_secret_service()?;
            crate::e2e::installed()?.check_paths(&paths, &manager_config.known_hosts)?;
            let _ = use_secret_service;
            false
        };
        let workspace = Workspace::open(paths.clone(), project)?;
        #[cfg(feature = "e2e-scenarios")]
        crate::e2e::installed()?.check_paths_resolved(&paths)?;
        let project_id = workspace.project_id().to_string();
        let mut stores = SecretStores::new();

        // 1. Keyring, when a Secret Service answers.
        let keyring = if use_secret_service {
            ninesql_secrets::detect_secret_service()
        } else {
            SecretServiceAvailability::Unavailable {
                detail: "not probed (disabled by configuration)".into(),
            }
        };
        register_keyring(&mut stores, &keyring, &project_id);

        // 2. The vault, always registered when it can be opened (starts locked).
        let vault = match Vault::open(&paths) {
            Ok(v) => {
                let v = Arc::new(v);
                v.set_idle_timeout(vault_idle_timeout);
                match VaultStore::new(Arc::clone(&v), &project_id) {
                    Ok(store) => {
                        stores.register(Arc::new(store));
                        VaultSlot::Open(v)
                    }
                    Err(e) => VaultSlot::Unavailable(e.to_string()),
                }
            }
            Err(e) => {
                tracing::warn!(
                    kind = e.kind(),
                    "the vault cannot be opened; it is not available in this session"
                );
                VaultSlot::Unavailable(e.to_string())
            }
        };
        let backend = match stores.store_for_new_secrets() {
            Some(StoreKind::Keyring) => SecretBackend::Keyring,
            Some(StoreKind::Vault) => SecretBackend::Vault,
            _ => SecretBackend::AskEveryTime,
        };
        tracing::info!(backend = ?backend, "secret backend selected");
        // Test build: the vault in the run directory or nothing.
        #[cfg(feature = "e2e-scenarios")]
        if backend != SecretBackend::Vault {
            return Err(crate::e2e::GuardError::NoVault.into());
        }

        // 3. The session manager.
        let manager = SessionManager::new(manager_config, registry, stores.clone())?;
        manager.set_workspace(workspace.clone())?;

        // 4. Lock hooks: the vault, the "ask every time" cache, and the UI event.
        let cache = Arc::new(SessionSecretCache::new());
        let hooks = Arc::new(LockHooks::new());
        if let VaultSlot::Open(v) = &vault {
            hooks.add(Arc::clone(v) as Arc<dyn LockHook>);
        }
        hooks.add(Arc::clone(&cache) as Arc<dyn LockHook>);
        hooks.add(Arc::new(EventHook(manager.event_sink())));

        // 5. Sleep and screen lock.
        #[cfg(feature = "desktop-lock")]
        let (watcher, watcher_error) = if watch_desktop_lock {
            match ninesql_secrets::DesktopLockWatcher::start(Arc::clone(&hooks) as Arc<dyn LockHook>)
            {
                Ok(w) => {
                    tracing::info!(
                        session_lock = w.watches_session_lock(),
                        sleep_delay = w.holds_sleep_delay(),
                        "watching sleep and screen lock"
                    );
                    (Some(w), None)
                }
                Err(e) => {
                    tracing::warn!(
                        kind = e.kind(),
                        "sleep and screen lock are not watched; idle timers only"
                    );
                    (None, Some(e.to_string()))
                }
            }
        } else {
            (
                None,
                Some("not started (disabled by configuration)".to_owned()),
            )
        };
        #[cfg(not(feature = "desktop-lock"))]
        let watcher_error = {
            let _ = watch_desktop_lock;
            Some(
                "this build does not watch sleep and screen lock (feature `desktop-lock` off)"
                    .to_owned(),
            )
        };

        // 6. The idle timer, on the driver runtime.
        let timer = spawn_idle_timer(
            &manager,
            match &vault {
                VaultSlot::Open(v) => Some(Arc::clone(v)),
                VaultSlot::Unavailable(_) => None,
            },
            Arc::clone(&cache),
            lock_check_interval,
        );

        // 7. Preferences, history, execution service.
        let (prefs, preference_issues) = load_preferences(&paths);
        let (history, history_error) = if open_history {
            open_history_db(&paths, &project_id, &prefs)
        } else {
            (
                None,
                Some("not opened (disabled by configuration)".to_owned()),
            )
        };
        let execution = ExecutionService::new(manager.clone(), history, prefs);

        // 8. Metadata service (P4-B), with the project's navigator filters. A filter file that
        // cannot be read is reported, never fatal: the navigator then shows everything.
        let (metadata, navigator_issues) = match MetadataService::with_workspace(
            manager.clone(),
            MetadataConfig::default(),
            workspace.clone(),
        ) {
            Ok((service, issues)) => (
                service,
                issues.iter().map(|i| i.error.to_string()).collect(),
            ),
            Err(e) => {
                tracing::warn!(
                    kind = e.kind(),
                    "navigator.json cannot be read; no navigator filter"
                );
                (
                    MetadataService::new(manager.clone(), MetadataConfig::default()),
                    vec![e.to_string()],
                )
            }
        };

        // 9. Transfer service (P5-C).
        let transfer =
            TransferService::new(manager.clone(), &paths).with_metadata(metadata.clone());

        // 10. Edit service (P5-A core): row editing, filtered queries, foreign-key navigation.
        let edit = EditService::new(manager.clone(), metadata.clone(), execution.clone());

        Ok(Self {
            inner: Arc::new(Inner {
                workspace,
                manager,
                stores,
                backend,
                keyring,
                vault,
                cache,
                hooks,
                #[cfg(feature = "desktop-lock")]
                watcher: Mutex::new(watcher),
                watcher_error,
                timer: Mutex::new(Some(timer)),
                execution,
                metadata,
                transfer,
                edit,
                navigator_issues,
                history_error,
                preference_issues,
                paths,
            }),
        })
    }

    /// The open project.
    #[must_use]
    pub fn workspace(&self) -> &Workspace {
        &self.inner.workspace
    }

    /// The session manager (open, trust host keys, sessions).
    #[must_use]
    pub fn sessions(&self) -> &SessionManager {
        &self.inner.manager
    }

    /// The session-level event channel ([`SessionManager::take_events`]): the first call takes
    /// it. It also carries [`CoreEvent::SecretsLocked`].
    #[must_use]
    pub fn take_events(&self) -> Option<CoreEvents> {
        self.inner.manager.take_events()
    }

    /// The registered drivers.
    #[must_use]
    pub fn registry(&self) -> &DriverRegistry {
        self.inner.manager.registry()
    }

    /// [`DriverRegistry::validate_connection`]: the one validation entry point of the save
    /// (P1-C) and import (P2-C) flows. Pure, never blocks.
    ///
    /// # Errors
    /// [`CoreError::NoDriver`], [`CoreError::InvalidConnection`].
    pub fn validate_connection(&self, connection: &Connection) -> Result<(), CoreError> {
        self.registry().validate_connection(connection)
    }

    /// The DBeaver importer of this session: the open project, the registered drivers, the
    /// secret stores (default store: [`Self::secret_backend`]) and the session manager's DBeaver
    /// roots. Its methods block; prefer [`Self::preview_import`] / [`Self::apply_import`].
    #[must_use]
    pub fn importer(&self) -> Importer {
        Importer::new(
            self.inner.workspace.clone(),
            self.registry().clone(),
            self.inner.stores.clone(),
            self.inner.manager.dbeaver_roots().clone(),
            self.inner.backend.store(),
        )
    }

    /// Runs `f` on the runtime's blocking pool and answers through a [`Reply`].
    fn blocking<T, F>(&self, what: &'static str, f: F) -> Reply<Result<T, ImportError>>
    where
        T: Send + 'static,
        F: FnOnce() -> Result<T, ImportError> + Send + 'static,
    {
        let (tx, reply) = Reply::channel();
        let task = self.inner.manager.runtime().handle().spawn_blocking(f);
        self.inner.manager.runtime().spawn(async move {
            let result = task
                .await
                .unwrap_or_else(|_| Err(ImportError::Runtime(format!("the {what} task failed"))));
            let _ = tx.send(result);
        });
        reply
    }

    /// DBeaver import, step 1 (P2-B / P2-C): reads `source` read-only (I4) and answers with
    /// the [`ImportPreview`] (no secret value, I1), on the blocking pool (I6). The source
    /// workspace becomes a DBeaver root of the session: its database files open read-only.
    ///
    /// Dropping the [`Reply`] before the answer cancels the preview: it stops between DBeaver
    /// projects and the partial result is dropped ([`Self::preview_import_cancellable`] adds an
    /// explicit cancel).
    pub fn preview_import(
        &self,
        source: ImportSource,
    ) -> Reply<Result<ImportPreview, ImportError>> {
        self.preview_import_cancellable(source).0
    }

    /// [`Self::preview_import`] with a cancel handle (P2-D M1, a "Cancel" button): after
    /// [`ImportCancel::cancel`], or once the [`Reply`] is dropped, the preview stops at the
    /// next check (before and after each DBeaver project) and answers
    /// [`ImportError::Cancelled`]. Nothing is written either way.
    pub fn preview_import_cancellable(
        &self,
        source: ImportSource,
    ) -> (Reply<Result<ImportPreview, ImportError>>, ImportCancel) {
        let importer = self.importer();
        let cancel = ImportCancel::new();
        let flag = cancel.clone();
        let (mut tx, reply) = Reply::channel();
        let task = self
            .inner
            .manager
            .runtime()
            .handle()
            .spawn_blocking(move || importer.preview_cancellable(&source, &flag));
        let on_drop = cancel.clone();
        self.inner.manager.runtime().spawn(async move {
            let mut task = task;
            let result = tokio::select! {
                result = &mut task => result,
                () = tx.closed() => {
                    // Nobody waits for it any more: stop the work, drop its result.
                    on_drop.cancel();
                    tracing::debug!("import preview abandoned; cancelled");
                    return;
                }
            };
            let result = result.unwrap_or_else(|_| {
                Err(ImportError::Runtime(
                    "the import preview task failed".into(),
                ))
            });
            if tx.send(result).is_err() {
                // The reply was dropped while the preview was finishing: the caller abandoned
                // it, so report it as cancelled like an abandon seen before completion (the
                // `select!` above cannot tell which came first when both are ready).
                on_drop.cancel();
            }
        });
        (reply, cancel)
    }

    /// DBeaver import, step 2: applies `preview` with the user's `choices`, on the blocking
    /// pool (I6). See [`Importer::apply`]. On [`ImportError::VaultLocked`], unlock the vault
    /// ([`Self::unlock_vault`]) and call it again with the same preview; on
    /// [`ImportError::VaultNotCreated`], create it ([`Self::create_vault`]) first; on
    /// [`ImportError::SourceChanged`], preview again.
    pub fn apply_import(
        &self,
        preview: ImportPreview,
        choices: ImportChoices,
    ) -> Reply<Result<ImportOutcome, ImportError>> {
        let importer = self.importer();
        let vault = self.vault().ok();
        self.blocking("import", move || {
            // A vault not created yet holds nothing and is not "locked": saving into it would
            // fail half-way, so the UI is asked to create it first.
            if choices.store == Some(StoreKind::Vault)
                && preview.secret_count() > 0
                && vault
                    .as_ref()
                    .is_some_and(|v| matches!(v.status(), Ok(VaultStatus::NotCreated)))
            {
                return Err(ImportError::VaultNotCreated);
            }
            importer.apply(&preview, &choices)
        })
    }

    /// The execution service: run statements and scripts, the query history, the preferences
    /// in effect (P3-C).
    #[must_use]
    pub fn execution(&self) -> &ExecutionService {
        &self.inner.execution
    }

    /// Exports the connections `ids` (in that order) to a **new** DBeaver `data-sources.json`
    /// file or a new DBeaver project folder (P5-D, F-CON-15), on the blocking pool (I6): no
    /// secret is written (I1), and a target inside a DBeaver location, the session's DBeaver
    /// roots included, is refused (I4). See [`crate::dbeaver_export`]. On
    /// [`DbeaverExportError::Changed`] nothing was written: ask for the target again.
    pub fn export_dbeaver(
        &self,
        ids: Vec<ConnectionId>,
        target: ninesql_dbeaver::export::ExportTarget,
    ) -> Reply<Result<DbeaverExportOutcome, DbeaverExportError>> {
        let (tx, reply) = Reply::channel();
        let workspace = self.inner.workspace.clone();
        let roots = self.inner.manager.dbeaver_roots().clone();
        let task = self
            .inner
            .manager
            .runtime()
            .handle()
            .spawn_blocking(move || {
                crate::dbeaver_export::export_dbeaver_blocking(&workspace, &roots, &ids, &target)
            });
        self.inner.manager.runtime().spawn(async move {
            let result = task.await.unwrap_or_else(|_| {
                Err(DbeaverExportError::Runtime("the export task failed".into()))
            });
            let _ = tx.send(result);
        });
        reply
    }

    /// The edit service (P5-A core): row editing with a DML preview and a transactional save,
    /// server-side sort and filter queries with a filter history, foreign-key navigation
    /// ([`crate::edit`]).
    #[must_use]
    pub fn edit(&self) -> &EditService {
        &self.inner.edit
    }

    /// The transfer service: export rows to files, preview and import CSV files (P5-C).
    #[must_use]
    pub fn transfer(&self) -> &TransferService {
        &self.inner.transfer
    }

    /// The metadata service (P4-B): navigator nodes, DDL, filters, search and completion
    /// snapshots ([`crate::metadata`]).
    #[must_use]
    pub fn metadata(&self) -> &MetadataService {
        &self.inner.metadata
    }

    /// Navigator filter entries dropped at startup (invalid or unreadable `navigator.json`),
    /// as presentable messages that never echo a pattern.
    #[must_use]
    pub fn navigator_filter_issues(&self) -> &[String] {
        &self.inner.navigator_issues
    }

    /// The query history, when it could be opened ([`Self::history_error`] says why not).
    #[must_use]
    pub fn history(&self) -> Option<&History> {
        self.inner.execution.history()
    }

    /// Why the query history is not available in this session (`None` when it is).
    #[must_use]
    pub fn history_error(&self) -> Option<&str> {
        self.inner.history_error.as_deref()
    }

    /// The execution preferences in effect (F-WS-04).
    #[must_use]
    pub fn preferences(&self) -> Preferences {
        self.inner.execution.preferences()
    }

    /// Invalid preference entries found in `settings.toml` at startup (their defaults are
    /// used), for the settings problems list.
    #[must_use]
    pub fn preference_issues(&self) -> &[PreferenceIssue] {
        &self.inner.preference_issues
    }

    /// Validates `changes`, writes them to `settings.toml` (atomically, keeping everything
    /// else, [`preferences::save_changes`]) on the blocking pool, then applies the preferences
    /// now in the file. Answers them. Nothing is applied when the write fails.
    pub fn save_preferences(
        &self,
        changes: Vec<PreferenceChange>,
    ) -> Reply<Result<Preferences, PreferencesError>> {
        // Checked first, so an invalid value fails without touching the file.
        let mut check = self.preferences();
        for c in &changes {
            if let Err(e) = c.apply(&mut check) {
                return Reply::ready(Err(e));
            }
        }
        let path = settings_target(&self.inner.paths);
        let execution = self.inner.execution.clone();
        let (tx, reply) = Reply::channel();
        let task = self
            .inner
            .manager
            .runtime()
            .handle()
            .spawn_blocking(move || preferences::save_changes(&path, &changes));
        self.inner.manager.runtime().spawn(async move {
            let result = match task.await {
                Ok(Ok(prefs)) => {
                    execution.set_preferences(prefs.clone());
                    Ok(prefs)
                }
                Ok(Err(e)) => Err(e),
                Err(_) => Err(PreferencesError::Unreadable("the save task failed")),
            };
            let _ = tx.send(result);
        });
        reply
    }

    /// The secret stores (for [`Workspace::create_connection_checked`], run on a background
    /// thread: store calls block).
    #[must_use]
    pub fn secret_stores(&self) -> &SecretStores {
        &self.inner.stores
    }

    /// Where newly saved secrets go: keyring, else vault, else "ask every time" only.
    #[must_use]
    pub fn secret_backend(&self) -> SecretBackend {
        self.inner.backend
    }

    /// What the Secret Service probe found at startup.
    #[must_use]
    pub fn keyring_availability(&self) -> &SecretServiceAvailability {
        &self.inner.keyring
    }

    /// Why no vault is available in this session (`None` when it is).
    #[must_use]
    pub fn vault_error(&self) -> Option<&str> {
        match &self.inner.vault {
            VaultSlot::Open(_) => None,
            VaultSlot::Unavailable(e) => Some(e),
        }
    }

    /// The "ask every time" cache (in memory only, cleared on lock).
    #[must_use]
    pub fn secret_cache(&self) -> &Arc<SessionSecretCache> {
        &self.inner.cache
    }

    /// Every lock hook of the session (the vault, the cache, the event hook).
    #[must_use]
    pub fn lock_hooks(&self) -> &Arc<LockHooks> {
        &self.inner.hooks
    }

    fn vault(&self) -> Result<Arc<Vault>, CoreError> {
        match &self.inner.vault {
            VaultSlot::Open(v) => Ok(Arc::clone(v)),
            VaultSlot::Unavailable(e) => Err(CoreError::NoVault(e.clone())),
        }
    }

    /// Runs `f` with the vault on the runtime's blocking pool.
    fn with_vault<T, F>(&self, f: F) -> Reply<Result<T, CoreError>>
    where
        T: Send + 'static,
        F: FnOnce(&Vault) -> Result<T, CoreError> + Send + 'static,
    {
        let vault = match self.vault() {
            Ok(v) => v,
            Err(e) => return Reply::ready(Err(e)),
        };
        let (tx, reply) = Reply::channel();
        let task = self
            .inner
            .manager
            .runtime()
            .handle()
            .spawn_blocking(move || f(&vault));
        self.inner.manager.runtime().spawn(async move {
            let result = task
                .await
                .unwrap_or_else(|_| Err(CoreError::Runtime("the vault task failed".into())));
            let _ = tx.send(result);
        });
        reply
    }

    /// State of the vault (not created, locked, unlocked).
    pub fn vault_status(&self) -> Reply<Result<VaultStatus, CoreError>> {
        self.with_vault(|v| Ok(v.status()?))
    }

    /// Unlocks the vault with its master password, on the blocking pool (Argon2id takes about a
    /// second). The password is moved in and dropped (zeroized) as soon as the key is derived;
    /// it is never logged or copied elsewhere (I1).
    ///
    /// # Errors
    /// [`CoreError::Vault`] with [`VaultError::WrongPassword`](ninesql_secrets::VaultError::WrongPassword),
    /// [`VaultError::TooManyAttempts`](ninesql_secrets::VaultError::TooManyAttempts),
    /// [`VaultError::NotCreated`](ninesql_secrets::VaultError::NotCreated) (call
    /// [`Self::create_vault`]) or a file error; [`CoreError::NoVault`].
    ///
    /// A lock (sleep, screen lock, "lock secrets") that arrives after this call, before the
    /// unlock is done, wins: the vault is locked again and the answer is
    /// [`VaultError::Locked`](ninesql_secrets::VaultError::Locked).
    pub fn unlock_vault(&self, master: SecretString) -> Reply<Result<(), CoreError>> {
        let hooks = Arc::clone(&self.inner.hooks);
        let generation = hooks.generation();
        self.with_vault(move |v| {
            let result = v.unlock(&master);
            drop(master);
            result?;
            relock_if_locked_since(v, &hooks, generation)
        })
    }

    /// Creates the vault (first use) with a master password and the default Argon2id
    /// parameters, and leaves it unlocked. The password is zeroized when this ends.
    ///
    /// # Errors
    /// [`CoreError::Vault`] with
    /// [`VaultError::AlreadyExists`](ninesql_secrets::VaultError::AlreadyExists) or
    /// [`VaultError::EmptyPassword`](ninesql_secrets::VaultError::EmptyPassword);
    /// [`CoreError::NoVault`].
    pub fn create_vault(&self, master: SecretString) -> Reply<Result<(), CoreError>> {
        self.create_vault_with(master, KdfParams::DEFAULT)
    }

    /// [`Self::create_vault`] with explicit Argon2id parameters. As with
    /// [`Self::unlock_vault`], a lock that arrives before the vault is created and unlocked
    /// leaves it created but locked, and the answer is
    /// [`VaultError::Locked`](ninesql_secrets::VaultError::Locked).
    ///
    /// # Errors
    /// As [`Self::create_vault`].
    pub fn create_vault_with(
        &self,
        master: SecretString,
        params: KdfParams,
    ) -> Reply<Result<(), CoreError>> {
        let hooks = Arc::clone(&self.inner.hooks);
        let generation = hooks.generation();
        self.with_vault(move |v| {
            let result = v.create(&master, params);
            drop(master);
            result?;
            relock_if_locked_since(v, &hooks, generation)
        })
    }

    /// Locks the vault and clears the "ask every time" cache now ("lock secrets" action), on
    /// the blocking pool (the vault may be busy deriving a key). Emits
    /// [`CoreEvent::SecretsLocked`] with [`SecretsLockReason::Requested`].
    pub fn lock_now(&self) -> Reply<()> {
        let hooks = Arc::clone(&self.inner.hooks);
        let (tx, reply) = Reply::channel();
        let task = self
            .inner
            .manager
            .runtime()
            .handle()
            .spawn_blocking(move || hooks.lock_all(LockTrigger::Requested));
        self.inner.manager.runtime().spawn(async move {
            let _ = task.await;
            let _ = tx.send(());
        });
        reply
    }

    /// Whether sleep and screen lock are watched. Never blocks.
    #[must_use]
    pub fn lock_watcher_status(&self) -> LockWatcherStatus {
        #[cfg(feature = "desktop-lock")]
        {
            let watcher = self
                .inner
                .watcher
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            match watcher.as_ref() {
                Some(w) => {
                    // Read once: the watcher changes state on its own thread.
                    let state = w.state();
                    let error = watcher_state_problem(state, w.session_lock_lost());
                    LockWatcherStatus {
                        built: true,
                        is_running: error.is_none(),
                        holds_sleep_delay: w.holds_sleep_delay(),
                        watches_session_lock: w.watches_session_lock(),
                        error,
                    }
                }
                None => LockWatcherStatus {
                    built: true,
                    is_running: false,
                    holds_sleep_delay: false,
                    watches_session_lock: false,
                    error: self.inner.watcher_error.clone(),
                },
            }
        }
        #[cfg(not(feature = "desktop-lock"))]
        {
            LockWatcherStatus {
                built: false,
                is_running: false,
                holds_sleep_delay: false,
                watches_session_lock: false,
                error: self.inner.watcher_error.clone(),
            }
        }
    }

    /// At exit: locks the secrets, stops the watcher and the timer, closes every session
    /// (waiting at most `timeout`) and stops the runtime. **Blocking**: call it from the main
    /// thread, never from an async context.
    pub fn shutdown(self, timeout: Duration) {
        self.inner.hooks.lock_all(LockTrigger::Requested);
        #[cfg(feature = "desktop-lock")]
        drop(
            self.inner
                .watcher
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take(),
        );
        if let Some(t) = self
            .inner
            .timer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            t.abort();
        }
        let manager = self.inner.manager.clone();
        drop(self);
        manager.shutdown(timeout);
    }
}

/// `settings.toml`, or its target when it is a symbolic link to a regular file of ours
/// (dotfile managers), as the UI resolves it.
fn settings_target(paths: &Paths) -> std::path::PathBuf {
    use std::os::unix::fs::MetadataExt as _;
    let path = paths.settings_file();
    let is_link = std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink());
    if is_link
        && let Ok(target) = std::fs::canonicalize(&path)
        && std::fs::symlink_metadata(&target)
            .is_ok_and(|m| m.file_type().is_file() && m.uid() == ninesql_storage::fs::current_uid())
    {
        return target;
    }
    path
}

/// The preferences of `settings.toml`; defaults (with one issue) when it cannot be read.
fn load_preferences(paths: &Paths) -> (Preferences, Vec<PreferenceIssue>) {
    match Preferences::load(&settings_target(paths)) {
        Ok(loaded) => loaded,
        Err(e) => {
            tracing::warn!(
                kind = e.kind(),
                "settings.toml cannot be read; default execution preferences"
            );
            (Preferences::default(), Vec::new())
        }
    }
}

/// Opens `history.sqlite` and starts its writer; the reason when it cannot.
fn open_history_db(
    paths: &Paths,
    project: &str,
    prefs: &Preferences,
) -> (Option<History>, Option<String>) {
    let opened = ninesql_storage::HistoryDb::open(&paths.history_file())
        .map_err(crate::history::HistoryError::from)
        .and_then(|db| History::start(db, project, &prefs.history));
    match opened {
        Ok(h) => (Some(h), None),
        Err(e) => {
            tracing::warn!(
                kind = e.kind(),
                "the query history is not available in this session"
            );
            (None, Some(e.to_string()))
        }
    }
}

#[cfg(feature = "secret-service")]
fn register_keyring(stores: &mut SecretStores, keyring: &SecretServiceAvailability, project: &str) {
    if keyring.is_available() {
        match ninesql_secrets::SecretServiceStore::new(project) {
            Ok(store) => stores.register(Arc::new(store)),
            // `InvalidProjectId`: a fixed kind, never the error's message (ADR-0029 §3.2).
            Err(_) => tracing::warn!(error = "invalid-project-id", "keyring store not registered"),
        }
    } else {
        // The availability's detail is the D-Bus message: not logged (ADR-0029 §3.2).
        tracing::info!("no Secret Service: the keyring is not used");
    }
}

#[cfg(not(feature = "secret-service"))]
fn register_keyring(_: &mut SecretStores, keyring: &SecretServiceAvailability, _: &str) {
    if keyring.is_available() {
        // Another crate enabled the backend in `ninesql-secrets`, not this one: not used.
        tracing::info!("a Secret Service answers, but this build of the core does not use it");
    }
}

/// Every `interval`: locks the vault if idle and drops expired cached prompts, on the blocking
/// pool (the vault mutex may be held by a key derivation). Emits [`CoreEvent::SecretsLocked`]
/// with [`SecretsLockReason::Idle`] when the vault locked.
fn spawn_idle_timer(
    manager: &SessionManager,
    vault: Option<Arc<Vault>>,
    cache: Arc<SessionSecretCache>,
    interval: Duration,
) -> JoinHandle<()> {
    let sink = manager.event_sink();
    let interval = interval.max(Duration::from_millis(100));
    manager.runtime().spawn(async move {
        let mut ticks = tokio::time::interval(interval);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticks.tick().await;
        loop {
            ticks.tick().await;
            let vault = vault.clone();
            let cache = Arc::clone(&cache);
            let locked = tokio::task::spawn_blocking(move || {
                cache.purge_expired();
                vault.is_some_and(|v| v.lock_if_idle())
            })
            .await
            .unwrap_or(false);
            if locked {
                tracing::info!("vault locked after its idle timeout");
                sink.emit(CoreEvent::SecretsLocked {
                    reason: SecretsLockReason::Idle,
                });
            }
        }
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).
mod tests {
    use super::*;

    /// The UI lists the watcher among the connection problems while `error` is set; it clears
    /// once the watcher is back (Phase 1 follow-up "Wave 3 (secrets)").
    #[cfg(feature = "desktop-lock")]
    #[test]
    fn the_watcher_problem_follows_its_state() {
        assert_eq!(watcher_state_problem(WatcherState::Watching, false), None);
        assert!(
            watcher_state_problem(WatcherState::Watching, true)
                .unwrap()
                .contains("screen lock")
        );
        for s in [
            WatcherState::Reconnecting,
            WatcherState::LogindAbsent,
            WatcherState::Stopped,
        ] {
            assert!(watcher_state_problem(s, false).is_some(), "{s:?}");
        }
        assert!(
            watcher_state_problem(WatcherState::Reconnecting, false)
                .unwrap()
                .contains("reconnecting")
        );
    }

    /// Review P1-D wiring, L2: a lock that lands after the unlock was asked for, before the
    /// unlock runs (while it is queued, so the vault had nothing to lock), wins.
    #[test]
    fn a_lock_between_the_unlock_request_and_the_unlock_wins() {
        let dir = tempfile::tempdir().unwrap();
        let vault = Arc::new(
            Vault::open_at(dir.path().join("vault.bin"), &dir.path().join("vault.lock")).unwrap(),
        );
        let master = SecretString::from("master-pw-0123".to_owned());
        vault
            .create(&master, KdfParams::insecure_for_tests())
            .unwrap();
        vault.lock();
        let hooks = LockHooks::new();
        hooks.add(Arc::clone(&vault) as Arc<dyn LockHook>);

        // Captured when the UI calls `unlock_vault`…
        let generation = hooks.generation();
        // …the screen locks while the task is queued: the vault is already locked, no-op…
        hooks.lock_all(LockTrigger::SessionLock);
        // …then the task unlocks.
        vault.unlock(&master).unwrap();
        let err = relock_if_locked_since(&vault, &hooks, generation).unwrap_err();
        assert!(
            matches!(err, CoreError::Vault(ninesql_secrets::VaultError::Locked)),
            "{err:?}"
        );
        assert!(!vault.is_unlocked());

        // Without a lock in between, the unlock stands.
        let generation = hooks.generation();
        vault.unlock(&master).unwrap();
        relock_if_locked_since(&vault, &hooks, generation).unwrap();
        assert!(vault.is_unlocked());
    }
}
