//! DBeaver import, core side (ROADMAP P2-C, ADR-0006, invariants I1, I3 and I4).
//!
//! `ninesql-dbeaver` parses a DBeaver workspace read-only into a mirror of the connection model
//! (P2-A). This module turns that mirror into saved NineSQL connections:
//!
//! 1. [`Importer::preview`] opens the source (known locations or a folder the user picked),
//!    runs [`ninesql_dbeaver::import_project`] on every project, converts each connection
//!    ([`Connection`], then `sanitize()`, `validate()` and
//!    [`DriverRegistry::validate_connection`]) and returns an [`ImportPreview`]: what would be
//!    created or updated, imported disabled (with the persisted [`DisabledReason`]) or not
//!    imported, the connection types, the folders, the secrets to move (slots and kinds, never
//!    values) and the report entries. The preview holds **no secret value** (I1). The source
//!    workspace is registered as a DBeaver root ([`DbeaverRoots::add`]), so its database files
//!    are opened read-only (I4), during a test-before-save too.
//! 2. [`Importer::apply`] reads the source again, checks it still gives the same preview
//!    (otherwise [`ImportError::SourceChanged`]: preview again), moves each selected secret to
//!    the chosen store (keyring → vault, never plaintext; the imported values are zeroized when
//!    dropped), merges the connection types (only ever making an existing type stricter,
//!    built-ins included), writes the connections in one checked bulk write that keeps the imported
//!    init SQL marker ([`Workspace`]), and saves the import report in the project
//!    (`imports/dbeaver-<timestamp>.json`, no secret).
//!
//! Both are **blocking** (file system, secret stores). The application calls them through
//! [`crate::CoreServices::preview_import`] and [`crate::CoreServices::apply_import`], which run
//! them on the driver runtime's blocking pool and answer through a [`crate::Reply`] (I6).
//!
//! Re-import is idempotent (ADR-0006 §8, F-CON-14): a connection whose `origin` matches the
//! DBeaver id, project **and** canonical source workspace is updated in place (same NineSQL id)
//! instead of duplicated; a match on the id alone is reported and imported as a new connection.
//! What the update keeps from the saved connection is listed on [`PlanAction::Update`].
//!
//! Every imported connection with init SQL gets `init_sql_needs_confirmation = true`, on a
//! re-import too, whatever the DBeaver file or the saved connection said (ADR-0022 §1): the
//! first open asks for confirmation of every init statement whatever the connection type.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use ninesql_dbeaver::mapping::{
    self as mirror, CredentialsStatus, ImportedConnection, ImportedConnectionType, SecretState,
};
use ninesql_dbeaver::{ImportIssue, InstallKind, IssueKind, SearchRoots, Severity};
use ninesql_secrets::{
    SecretError, SecretId, SecretKind, SecretMeta, SecretRef, SecretStores, StoreKind,
};
use ninesql_storage::document::{self, Document, Format};
use ninesql_storage::fs::ensure_private_dir;
use serde::{Deserialize, Serialize};

use crate::connection::{
    Auth, ClientCertificate, Connection, ConnectionId, DisabledKind, DisabledReason, Endpoint,
    EngineId, Origin, SecretSlot, SecretSource, SshAuth, SshHop, SshTunnel, TlsConfig, TlsMode,
};
use crate::connection_type::{
    Color, ConnectionType, ConnectionTypeId, ConnectionTypes, SafetyPolicy,
};
use crate::dbeaver_roots::DbeaverRoots;
use crate::error::{CoreError, WorkspaceError};
use crate::registry::DriverRegistry;
use crate::workspace::{Connections, Workspace};

// ---------------------------------------------------------------------------------------------
// Public model

/// Where to import from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportSource {
    /// The known DBeaver locations under a home directory (native, Flatpak, Snap; ADR-0006 §2).
    Detect(SearchRoots),
    /// A folder the user picked: a DBeaver workspace (its projects are listed) or one project.
    Folder(PathBuf),
}

impl ImportSource {
    /// [`ImportSource::Detect`] under `$HOME`; `None` when `HOME` is unset or relative.
    #[must_use]
    pub fn detect_from_env() -> Option<Self> {
        SearchRoots::from_env().map(Self::Detect)
    }
}

/// A DBeaver workspace found in the source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewWorkspace {
    /// How DBeaver was installed, from the location.
    pub kind: InstallKind,
    /// Workspace folder.
    pub path: PathBuf,
    /// `product-version` of the DBeaver build that last wrote it, when readable.
    pub product_version: Option<String>,
}

/// Import status of one DBeaver connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ImportStatus {
    /// Imported and enabled.
    Imported,
    /// Imported disabled: see [`Connection::disabled_reason`].
    ImportedDisabled,
    /// Not imported.
    Failed,
}

/// What applying the preview does with one DBeaver connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanAction {
    /// A new NineSQL connection.
    Create,
    /// Updates the connection already imported from the same DBeaver id, **project and source
    /// workspace** (F-CON-14; a match on the id alone is a conflict and gives a
    /// [`PlanAction::Create`]). Kept from the saved connection: its id, name, description,
    /// folder, password-exchange policy, per-hop SSH host key policies (same host and port), a
    /// stricter read-only or manual-commit override, a stricter TLS mode and the saved CA file
    /// (unless [`ImportChoices::accept_tls_changes`] lists the connection), a stricter
    /// connection type, a "disabled by the user" state, and saved secrets of fields DBeaver has
    /// no value for **only while everything they authenticate against is unchanged**
    /// ([`SecretChangeKind`]). Taken from DBeaver: the endpoint, engine, user names, SSH hops,
    /// driver properties, init SQL (marked again as needing confirmation), default schema /
    /// catalog and auto-commit. `diff` lists what changes, for the preview screen.
    Update {
        /// The saved connection.
        existing: ConnectionId,
        /// What the update changes.
        diff: Box<UpdateDiff>,
    },
    /// Not imported (the parser failed it, or its converted settings break a model rule).
    NotImported {
        /// Why (no value from the file).
        reason: String,
    },
}

/// A saved value and the imported one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change<T> {
    /// In the saved connection.
    pub saved: T,
    /// From the DBeaver file.
    pub imported: T,
}

/// A TLS difference on re-import.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsChange {
    /// Mode and CA file of the saved connection.
    pub saved: (TlsMode, Option<PathBuf>),
    /// Mode and CA file from the DBeaver file.
    pub imported: (TlsMode, Option<PathBuf>),
    /// The saved settings are kept (the imported mode is weaker, or the CA file differs): the
    /// imported ones apply only if [`ImportChoices::accept_tls_changes`] lists the connection.
    pub kept_saved: bool,
}

/// What a re-import does with a secret the saved connection references.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SecretChangeKind {
    /// Kept: DBeaver has no value for the field and what it authenticates against (engine,
    /// host, port, user, TLS mode and CA file, SSH route; for an SSH slot, the hop's host, port
    /// and user) is unchanged.
    Kept,
    /// Replaced by the value DBeaver saved (when a store is chosen; with no store, kept if it
    /// could be kept, else dropped).
    Replaced,
    /// Dropped: the field is gone or what it authenticates against changed. The secret is
    /// deleted after the import is written; NineSQL asks for it at connect.
    Dropped,
}

/// One saved secret of an updated connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SecretChange {
    /// Credential field.
    pub slot: SecretSlot,
    /// What happens to it.
    pub change: SecretChangeKind,
}

/// Field-level differences of a re-import ([`PlanAction::Update`]), for the preview screen.
/// `None` / empty means unchanged. Never holds a secret value.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UpdateDiff {
    /// Engine.
    pub engine: Option<Change<EngineId>>,
    /// Endpoint (host, port, database or file).
    pub endpoint: Option<Change<Endpoint>>,
    /// Database user name.
    pub username: Option<Change<Option<String>>>,
    /// TLS mode / CA file.
    pub tls: Option<TlsChange>,
    /// SSH hops as `user@host:port`, jump hosts first.
    pub ssh: Option<Change<Vec<String>>>,
    /// Connection type, and whether the saved one is kept (the imported one is weaker).
    pub connection_type: Option<(Change<ConnectionTypeId>, bool)>,
    /// Saved secrets: kept, replaced or dropped.
    pub secrets: Vec<SecretChange>,
    /// Driver properties (non-secret engine options such as `search_path`).
    pub properties: Option<Change<BTreeMap<String, String>>>,
    /// Init SQL statements (marked for confirmation again in any case).
    pub init_sql: Option<Change<Vec<String>>>,
}

/// A secret DBeaver saved that the import moves to the secret store (never the value, I1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SecretToMove {
    /// Credential field of the connection.
    pub slot: SecretSlot,
    /// Kind of secret.
    pub kind: SecretKind,
}

/// The plan for one DBeaver connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionPlan {
    /// DBeaver project.
    pub project: String,
    /// DBeaver data source id (`origin.dbeaver_id`).
    pub dbeaver_id: String,
    /// Connection name, for display.
    pub name: String,
    /// Status after the import.
    pub status: ImportStatus,
    /// Create, update or not imported.
    pub action: PlanAction,
    /// The connection as it will be written, without secret values: credential fields DBeaver
    /// saved a value for are [`SecretSource::Prompt`] here and point at the store once applied.
    /// It carries `init_sql_needs_confirmation = true` whenever it has init SQL, so an
    /// [`OpenRequest`](crate::OpenRequest) built from it for a test-before-save asks for
    /// confirmation (`imported = true`). `None` when not imported.
    pub connection: Option<Connection>,
    /// Secrets to move to the store.
    pub secrets: Vec<SecretToMove>,
    /// Report entries of this connection (parser and core).
    pub issues: Vec<ImportIssue>,
}

/// A folder of the source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderPlan {
    /// Path, outermost first.
    pub path: Vec<String>,
    /// Description (not kept: NineSQL folders have none).
    pub description: Option<String>,
    /// Whether an imported connection lives in it. NineSQL folders exist through their
    /// connections: an empty folder is listed but not created.
    pub used: bool,
}

/// What the import does with one DBeaver connection type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeAction {
    /// A new user-defined type.
    Create(ConnectionType),
    /// A type of the same id exists (a built-in `dev` / `test` / `prod` included) and the
    /// DBeaver one is stricter on some point: the existing type is made stricter there, never
    /// weaker; its name and color stay (P2-C review, M3).
    Tighten(ConnectionType),
    /// A type of the same id exists and is at least as strict: unchanged.
    Keep,
}

/// The plan for one DBeaver connection type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypePlan {
    /// DBeaver id.
    pub dbeaver_id: String,
    /// NineSQL id.
    pub id: ConnectionTypeId,
    /// Display name in the source.
    pub name: String,
    /// What happens.
    pub action: TypeAction,
}

/// The plan for one DBeaver project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectPreview {
    /// DBeaver project name.
    pub name: String,
    /// Project folder.
    pub path: PathBuf,
    /// DBeaver workspace holding it (registered as a DBeaver root, I4).
    pub dbeaver_workspace: PathBuf,
    /// What happened to `credentials-config.json`.
    pub credentials: CredentialsStatus,
    /// Folders.
    pub folders: Vec<FolderPlan>,
    /// Connections, sorted by DBeaver id.
    pub connections: Vec<ConnectionPlan>,
    /// Report entries not tied to one connection.
    pub issues: Vec<ImportIssue>,
}

/// The result of [`Importer::preview`]: everything the import would do, without a secret
/// value (I1). Pass it back unchanged to [`Importer::apply`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportPreview {
    /// The source, read again by [`Importer::apply`].
    pub source: ImportSource,
    /// Workspaces found.
    pub workspaces: Vec<PreviewWorkspace>,
    /// Projects, in workspace then name order.
    pub projects: Vec<ProjectPreview>,
    /// Connection types of every project, folded together (a type id defined twice keeps the
    /// stricter rules of both) and planned against the project's types, built-ins included
    /// (only ever made stricter).
    pub connection_types: Vec<TypePlan>,
    /// Report entries not tied to one project: connection types, and the source-level entries
    /// of the read (duplicate folders read once, whole-source limits reached).
    pub issues: Vec<ImportIssue>,
    /// Where secrets go by default (the session's backend); `None`: no store, saved passwords
    /// are not imported and NineSQL asks for them at connect.
    pub default_store: Option<StoreKind>,
}

impl ImportPreview {
    /// Every connection plan, all projects together.
    pub fn connections(&self) -> impl Iterator<Item = &ConnectionPlan> {
        self.projects.iter().flat_map(|p| p.connections.iter())
    }

    /// A whole-source limit was reached or a folder listing was cut (P2-D M1): the source may
    /// hold more than this preview shows. The UI says so; the [`IssueKind::Truncated`] entries
    /// of [`Self::issues`] and of the projects say what was left out.
    #[must_use]
    pub fn truncated(&self) -> bool {
        self.issues
            .iter()
            .chain(self.projects.iter().flat_map(|p| p.issues.iter()))
            .any(|i| i.kind == IssueKind::Truncated)
    }

    /// Number of secrets the import would move.
    #[must_use]
    pub fn secret_count(&self) -> usize {
        self.connections().map(|c| c.secrets.len()).sum()
    }

    /// The connection to use for a test-before-save of the plan of `dbeaver_id`: the converted
    /// connection with the imported init SQL marker set when it has init SQL (ADR-0022 §1; a
    /// new connection is not on disk yet, so the core uses the request's marker) and **every
    /// credential field on "ask"**: no saved secret of a connection being updated is used
    /// against settings the user has not accepted yet (P2-C review, H1). The open asks for them
    /// ([`crate::OpenOutcome::NeedsSecrets`]). `None` when not imported or disabled.
    #[must_use]
    pub fn connection_for_test(&self, dbeaver_id: &str) -> Option<Connection> {
        let mut c = self
            .connections()
            .find(|c| c.dbeaver_id == dbeaver_id)?
            .connection
            .clone()?;
        clear_secret_refs(&mut c);
        c.enabled.then_some(c)
    }

    /// `current` with the type changes of this preview applied (what
    /// [`Importer::apply`] would save), for the policy of a test-before-save.
    #[must_use]
    pub fn types_after(&self, current: &ConnectionTypes) -> ConnectionTypes {
        let mut types = current.clone();
        for t in &self.connection_types {
            if let TypeAction::Create(t) | TypeAction::Tighten(t) = &t.action {
                types.upsert(t.clone());
            }
        }
        types
    }
}

/// What the user chose in the preview.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ImportChoices {
    /// Store for the imported secrets; `None`: do not move them (the fields ask at connect).
    /// There is no plaintext option (I1).
    pub store: Option<StoreKind>,
    /// DBeaver ids not to import.
    pub skip: BTreeSet<String>,
    /// Update connections imported before (`true`, default), or leave them as they are.
    pub update_existing: bool,
    /// DBeaver ids of updated connections whose imported TLS mode / CA file the user accepted
    /// although they are weaker or different ([`TlsChange::kept_saved`]). Empty by default:
    /// the saved TLS settings are kept.
    pub accept_tls_changes: BTreeSet<String>,
}

impl ImportChoices {
    /// Everything, secrets to `store`, existing connections updated.
    #[must_use]
    pub fn all(store: Option<StoreKind>) -> Self {
        Self {
            store,
            skip: BTreeSet::new(),
            update_existing: true,
            accept_tls_changes: BTreeSet::new(),
        }
    }
}

/// What [`Importer::apply`] did.
#[derive(Debug)]
#[non_exhaustive]
pub struct ImportOutcome {
    /// Connections created.
    pub created: usize,
    /// Connections updated (re-import).
    pub updated: usize,
    /// Of those, imported disabled.
    pub disabled: usize,
    /// Not imported (failed in the preview).
    pub failed: usize,
    /// Skipped by the user's choices.
    pub skipped: usize,
    /// Secrets moved to the store.
    pub secrets_moved: usize,
    /// Connection types created or tightened.
    pub types_changed: usize,
    /// The import report saved in the project; `None` when it could not be written (see
    /// `report_error`). The import itself is applied either way (P2-D review, L1).
    pub report_file: Option<PathBuf>,
    /// Why the report could not be saved ("report not saved: <reason>"), for the outcome
    /// screen. Never holds a secret: the reason is a storage error (path and OS error).
    pub report_error: Option<String>,
    /// The connections of the project as written (reload the UI list from it).
    pub connections: Connections,
}

/// Why an import could not run.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ImportError {
    /// The source folder cannot be used.
    #[error(transparent)]
    Source(#[from] ninesql_dbeaver::DbeaverError),
    /// No DBeaver workspace with a project in the known locations.
    #[error("no DBeaver workspace was found in the known locations; pick its folder")]
    NothingFound,
    /// The DBeaver files or the NineSQL connections changed since the preview.
    #[error(
        "the DBeaver workspace or the saved connections changed since the preview; preview the import again"
    )]
    SourceChanged,
    /// The chosen store is the vault and it is locked: unlock it
    /// ([`crate::CoreServices::unlock_vault`]) and apply again. Nothing was written.
    #[error("the vault is locked; unlock it and import again")]
    VaultLocked,
    /// The chosen store is the vault and it is not created yet: create it
    /// ([`crate::CoreServices::create_vault`]) and apply again. Nothing was written.
    #[error("the vault is not created yet; create it and import again")]
    VaultNotCreated,
    /// The chosen store is not available in this session.
    #[error("the {0} is not available in this session")]
    NoStore(StoreKind),
    /// A secret could not be stored (nothing was written to `connections.json`).
    #[error("cannot store an imported secret: {0}")]
    Secret(#[from] SecretError),
    /// Workspace files could not be read or written.
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
    /// The folder listing was cut (too many entries, the detection budget spent) before any
    /// DBeaver project could be read. Pick the DBeaver workspace folder, or one project folder,
    /// directly.
    #[error(
        "the folder holds too many entries to look for DBeaver projects in it; pick the DBeaver workspace folder or a project folder directly"
    )]
    ListingTruncated,
    /// The preview was cancelled ([`ImportCancel::cancel`], or its reply was dropped). Nothing
    /// was written.
    #[error("the import preview was cancelled")]
    Cancelled,
    /// The blocking task failed.
    #[error("import task: {0}")]
    Runtime(String),
}

/// Cancels a running import preview (P2-D M1). Cheap to clone; every clone shares one flag.
///
/// The flag is an `Arc<AtomicBool>`, so the DBeaver reader's own cancel flag can share it
/// (`ninesql_dbeaver::CancelFlag::from_shared`, see [`ImportCancel::shared`]).
#[derive(Debug, Clone, Default)]
pub struct ImportCancel(std::sync::Arc<std::sync::atomic::AtomicBool>);

impl ImportCancel {
    /// A flag not cancelled yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Asks the preview to stop. Idempotent, never blocks.
    pub fn cancel(&self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Whether [`Self::cancel`] was called.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The shared flag, for the DBeaver reader's cancel hook.
    #[must_use]
    pub fn shared(&self) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
        std::sync::Arc::clone(&self.0)
    }

    fn check(&self) -> Result<(), ImportError> {
        if self.is_cancelled() {
            Err(ImportError::Cancelled)
        } else {
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Importer

/// Runs the import against one project of the NineSQL workspace. **Blocking** methods: call
/// them on a blocking thread, or use the `CoreServices` wrappers.
#[derive(Debug, Clone)]
pub struct Importer {
    workspace: Workspace,
    registry: DriverRegistry,
    stores: SecretStores,
    roots: DbeaverRoots,
    default_store: Option<StoreKind>,
}

/// Secrets of one connection, by core slot, held only during [`Importer::apply`].
type SecretPayload =
    BTreeMap<(String, String), Vec<(SecretSlot, SecretKind, ninesql_secrets::SecretString)>>;

impl Importer {
    /// An importer writing to `workspace`, validating with `registry`, storing secrets in
    /// `stores` (default store `default_store`) and registering DBeaver roots in `roots` (share
    /// the session manager's, [`crate::SessionManager::dbeaver_roots`]).
    #[must_use]
    pub fn new(
        workspace: Workspace,
        registry: DriverRegistry,
        stores: SecretStores,
        roots: DbeaverRoots,
        default_store: Option<StoreKind>,
    ) -> Self {
        Self {
            workspace,
            registry,
            stores,
            roots,
            default_store,
        }
    }

    /// Builds the preview. **Blocking.** Reads DBeaver files read-only (I4) and the project's
    /// `connections.json` / `connection-types.json`; writes nothing but may add DBeaver roots.
    ///
    /// # Errors
    /// [`ImportError::Source`], [`ImportError::NothingFound`], [`ImportError::Workspace`].
    pub fn preview(&self, source: &ImportSource) -> Result<ImportPreview, ImportError> {
        self.preview_cancellable(source, &ImportCancel::new())
    }

    /// [`Self::preview`], stopping with [`ImportError::Cancelled`] once `cancel` is set: the
    /// DBeaver reader checks it during detection, folder listing and every project read, and the
    /// core before planning each project. Secrets already read are dropped (zeroized) with the
    /// partial result. **Blocking.**
    ///
    /// # Errors
    /// As [`Self::preview`], and [`ImportError::Cancelled`].
    pub fn preview_cancellable(
        &self,
        source: &ImportSource,
        cancel: &ImportCancel,
    ) -> Result<ImportPreview, ImportError> {
        let (preview, payload, _) = self.plan(source, &BTreeMap::new(), cancel)?;
        drop(payload);
        cancel.check()?;
        Ok(preview)
    }

    /// Applies a preview with the user's choices. **Blocking.** See the module documentation.
    ///
    /// Every imported secret is written under a fresh reference (`<id>:<slot>:<uuid>`); the
    /// connections switch to the new references in one bulk write, and only then are the
    /// references they replaced deleted. A failure therefore deletes only the new references
    /// and leaves every saved secret as it was (P2-C review, M1).
    ///
    /// # Errors
    /// [`ImportError::SourceChanged`] (preview again; also when a connection to update was
    /// edited since the preview), [`ImportError::VaultLocked`], [`ImportError::NoStore`],
    /// [`ImportError::Secret`], [`ImportError::Workspace`], and the source errors of
    /// [`Self::preview`].
    pub fn apply(
        &self,
        preview: &ImportPreview,
        choices: &ImportChoices,
    ) -> Result<ImportOutcome, ImportError> {
        let ids: BTreeMap<String, ConnectionId> = preview
            .connections()
            .filter_map(|c| Some((c.dbeaver_id.clone(), c.connection.as_ref()?.id)))
            .collect();
        let (fresh, mut payload, existing) =
            self.plan(&preview.source, &ids, &ImportCancel::new())?;
        if fresh != *preview {
            return Err(ImportError::SourceChanged);
        }
        let selected = |c: &ConnectionPlan| {
            c.connection.is_some()
                && !choices.skip.contains(&c.dbeaver_id)
                && (choices.update_existing || !matches!(c.action, PlanAction::Update { .. }))
        };
        if let Some(store) = choices.store {
            if !self.stores.has(store) {
                return Err(ImportError::NoStore(store));
            }
            let needs_store = fresh
                .connections()
                .any(|c| selected(c) && !c.secrets.is_empty());
            if needs_store && self.stores.store(store)?.is_locked() {
                return Err(ImportError::VaultLocked);
            }
        }

        let mut stored: Vec<SecretRef> = Vec::new();
        let mut upserts: Vec<(Connection, Option<Connection>)> = Vec::new();
        let mut entries = Vec::new();
        let (mut created, mut updated, mut disabled, mut failed, mut skipped, mut moved) =
            (0, 0, 0, 0, 0, 0);
        let result = (|| {
            for project in &fresh.projects {
                let mut report_connections = Vec::new();
                for plan in &project.connections {
                    let key = (plan.project.clone(), plan.dbeaver_id.clone());
                    let secrets = payload.remove(&key).unwrap_or_default();
                    let Some(connection) = &plan.connection else {
                        failed += 1;
                        report_connections.push(ReportConnection::from_plan(plan, None, 0));
                        continue;
                    };
                    if !selected(plan) {
                        skipped += 1;
                        drop(secrets);
                        report_connections.push(ReportConnection::skipped(plan));
                        continue;
                    }
                    let mut connection = connection.clone();
                    let mut extra = Vec::new();
                    let previous = match &plan.action {
                        PlanAction::Update { existing: id, diff } => {
                            let old = existing.get(*id).ok_or(ImportError::SourceChanged)?;
                            if choices.accept_tls_changes.contains(&plan.dbeaver_id) {
                                self.accept_tls(old, &mut connection, diff, &mut extra);
                            }
                            Some(old)
                        }
                        _ => None,
                    };
                    let mut count = 0;
                    for (slot, kind, value) in secrets {
                        match choices.store {
                            Some(store) => {
                                let reference = fresh_secret_ref(store, connection.id, slot)?;
                                let meta = SecretMeta {
                                    connection_id: connection.id.to_string(),
                                    kind,
                                    label: format!("NineSQL: {}", connection.name),
                                };
                                self.stores.save(&reference, &meta, &value)?;
                                drop(value);
                                stored.push(reference.clone());
                                connection
                                    .set_secret_ref(slot, reference)
                                    .map_err(WorkspaceError::from)?;
                                count += 1;
                            }
                            None => {
                                // No store chosen: the value is dropped (zeroized). A secret
                                // saved in NineSQL for this field stays if it still applies
                                // (P2-C review, L1).
                                drop(value);
                                if let Some(old) = previous
                                    && let Some((_, _, r)) = old
                                        .secret_refs()
                                        .into_iter()
                                        .find(|(s, k, _)| *s == slot && *k == kind)
                                    && secret_still_applies(old, &connection, slot)
                                {
                                    let _ = connection.set_secret_ref(slot, r.clone());
                                }
                            }
                        }
                    }
                    moved += count;
                    match plan.action {
                        PlanAction::Update { .. } => updated += 1,
                        _ => created += 1,
                    }
                    if !connection.enabled {
                        disabled += 1;
                    }
                    let mut entry = ReportConnection::from_plan(plan, Some(&connection), count);
                    entry.issues.extend(extra.iter().map(ReportIssue::from));
                    report_connections.push(entry);
                    upserts.push((connection, previous.cloned()));
                }
                entries.push(ReportProject::new(project, report_connections));
            }
            // Types first: a connection never names a type that is not saved. Only new or
            // stricter types are written, so a later failure never leaves a weaker one.
            let mut types = self.workspace.load_connection_types()?;
            let mut types_changed = 0;
            for t in &fresh.connection_types {
                if let TypeAction::Create(t) | TypeAction::Tighten(t) = &t.action {
                    types.upsert(t.clone());
                    types_changed += 1;
                }
            }
            if types_changed > 0 {
                self.workspace.save_connection_types(&types)?;
            }
            let written = self
                .workspace
                .save_imported(upserts.clone(), &self.registry)?
                .ok_or(ImportError::SourceChanged)?;
            Ok::<_, ImportError>((written, types_changed))
        })();
        let (written, types_changed) = match result {
            Ok(w) => w,
            Err(e) => {
                for reference in &stored {
                    if let Err(del) = self.stores.delete(reference) {
                        tracing::warn!(reference = %reference, error = %del, "could not roll back an imported secret");
                    }
                }
                return Err(e);
            }
        };
        drop(payload);

        // References the written connections no longer use (replaced or dropped).
        for (c, old) in &upserts {
            let Some(old) = old else {
                continue;
            };
            let now: Vec<&SecretRef> = c.secret_refs().into_iter().map(|(_, _, r)| r).collect();
            for (_, _, r) in old.secret_refs() {
                if !now.contains(&r)
                    && let Err(e) = self.stores.delete(r)
                {
                    tracing::warn!(reference = %r, error = %e, "could not delete a replaced secret");
                }
            }
        }

        let report = ImportReportFile {
            created_at: unix_now(),
            sources: fresh.workspaces.iter().map(|w| w.path.clone()).collect(),
            store: choices.store,
            issues: fresh.issues.iter().map(ReportIssue::from).collect(),
            projects: entries,
        };
        // connections.json is written and replaced secrets are deleted: failing here would leave
        // the caller with a stale list, whose next save would drop the imported connections. The
        // report is best effort (P2-D review, L1).
        let (report_file, report_error) = match self.save_report(&report) {
            Ok(path) => (Some(path), None),
            Err(e) => {
                tracing::warn!(
                    project = %self.workspace.project(),
                    error = %e,
                    "the DBeaver import report could not be saved; the import is applied"
                );
                (None, Some(format!("report not saved: {e}")))
            }
        };
        tracing::info!(
            project = %self.workspace.project(),
            created,
            updated,
            disabled,
            failed,
            skipped,
            secrets = moved,
            "DBeaver import applied"
        );
        Ok(ImportOutcome {
            created,
            updated,
            disabled,
            failed,
            skipped,
            secrets_moved: moved,
            types_changed,
            report_file,
            report_error,
            connections: written,
        })
    }

    /// The user accepted the imported TLS settings of an update: they replace the kept saved
    /// ones if the driver accepts them (otherwise the saved ones stay, with a report entry), and
    /// a kept secret whose TLS changed is dropped.
    fn accept_tls(
        &self,
        old: &Connection,
        c: &mut Connection,
        diff: &UpdateDiff,
        issues: &mut Vec<ImportIssue>,
    ) {
        let Some(tls) = diff.tls.as_ref().filter(|t| t.kept_saved) else {
            return;
        };
        let saved = c.tls.clone();
        c.tls.mode = tls.imported.0;
        c.tls.ca_file.clone_from(&tls.imported.1);
        if c.enabled && self.registry.validate_connection(c).is_err() {
            c.tls = saved;
            issues.push(issue(
                Severity::Warning,
                IssueKind::TlsConverted,
                None,
                "the accepted TLS settings are refused by the driver: the saved ones are kept",
            ));
            return;
        }
        issues.push(issue(
            Severity::Warning,
            IssueKind::TlsNotVerified,
            None,
            "the imported TLS settings replace the saved ones, as accepted",
        ));
        let kept: Vec<SecretSlot> = c.secret_refs().into_iter().map(|(s, _, _)| s).collect();
        for slot in kept {
            if !secret_still_applies(old, c, slot) {
                clear_secret_ref(c, slot);
            }
        }
    }

    /// Writes the report as `imports/dbeaver-<timestamp>.json` in the project directory (a new
    /// file; `-2`, `-3`… when the name is taken).
    fn save_report(&self, report: &ImportReportFile) -> Result<PathBuf, ImportError> {
        let dir = self.workspace.project_paths().dir().join("imports");
        ensure_private_dir(&dir).map_err(WorkspaceError::from)?;
        let stamp = timestamp(report.created_at);
        let mut path = dir.join(format!("dbeaver-{stamp}.json"));
        let mut n = 2;
        while path.exists() {
            path = dir.join(format!("dbeaver-{stamp}-{n}.json"));
            n += 1;
        }
        document::save(&path, report).map_err(WorkspaceError::from)?;
        Ok(path)
    }

    /// Reads the source and computes the preview, with the secrets of each connection on the
    /// side and the saved connections it was planned against. `ids` fixes the NineSQL id of
    /// new connections (from an earlier preview).
    fn plan(
        &self,
        source: &ImportSource,
        ids: &BTreeMap<String, ConnectionId>,
        cancel: &ImportCancel,
    ) -> Result<(ImportPreview, SecretPayload, Connections), ImportError> {
        cancel.check()?;
        // One flag for the whole read: detection, folder listing and project reads stop when
        // the preview is cancelled (P2-D M1).
        let flag = ninesql_dbeaver::CancelFlag::from_shared(cancel.shared());
        let dbeaver = |e: ninesql_dbeaver::DbeaverError| match e {
            ninesql_dbeaver::DbeaverError::Cancelled => ImportError::Cancelled,
            other => ImportError::Source(other),
        };
        let workspaces = match source {
            ImportSource::Detect(roots) => {
                let found =
                    ninesql_dbeaver::detect_workspaces_with(roots, &flag).map_err(dbeaver)?;
                if found.is_empty() {
                    return Err(ImportError::NothingFound);
                }
                found
            }
            ImportSource::Folder(path) => {
                vec![ninesql_dbeaver::open_folder_with(path, &flag).map_err(dbeaver)?]
            }
        };
        // Every folder becomes a DBeaver root before anything is read (I4).
        for w in &workspaces {
            self.add_root(&w.path);
            for p in &w.projects {
                self.add_root(&p.workspace);
            }
        }
        let listing_cut = workspaces.iter().any(|w| w.listing_truncated);
        let existing = self.workspace.load_connections()?.connections;
        let current_types = self.workspace.load_connection_types()?;

        // Every project is read once, under the whole-source limits, before anything is
        // planned, so the connection types of all of them (all detected workspaces included)
        // are folded before any connection is planned (review, M2).
        let read = ninesql_dbeaver::import_source(workspaces, &flag).map_err(dbeaver)?;
        cancel.check()?;
        if read.projects.is_empty() {
            // Never an empty preview without a reason.
            return Err(if read.truncated || listing_cut {
                ImportError::ListingTruncated
            } else {
                ImportError::NothingFound
            });
        }
        let ninesql_dbeaver::SourceImport {
            workspaces,
            projects: imports,
            issues: source_issues,
            truncated,
        } = read;
        let mut issues = source_issues;
        // The cut is always reported, so `ImportPreview::truncated` sees it.
        let reported = issues.iter().any(|i| i.kind == IssueKind::Truncated)
            || imports
                .iter()
                .flat_map(|p| p.import.issues.iter())
                .any(|i| i.kind == IssueKind::Truncated);
        if (truncated || listing_cut) && !reported {
            issues.push(issue(
                Severity::Warning,
                IssueKind::Truncated,
                None,
                "the DBeaver source holds more than NineSQL reads in one import: some projects or connections were not read",
            ));
        }
        let all_types: Vec<&ImportedConnectionType> = imports
            .iter()
            .flat_map(|p| p.import.connection_types.iter())
            .collect();
        let connection_types = plan_types(&all_types, &current_types, &mut issues);
        let mut types_after = current_types.clone();
        for t in &connection_types {
            if let TypeAction::Create(t) | TypeAction::Tighten(t) = &t.action {
                types_after.upsert(t.clone());
            }
        }

        let mut payload = SecretPayload::new();
        let mut projects = Vec::new();
        let mut seen_ids = BTreeSet::new();
        for ninesql_dbeaver::SourceProject { project, import } in imports {
            cancel.check()?;
            let (preview, secrets) = self.plan_project(
                &project,
                import,
                &existing,
                &types_after,
                ids,
                &mut seen_ids,
            );
            payload.extend(secrets);
            projects.push(preview);
        }
        let preview = ImportPreview {
            source: source.clone(),
            workspaces: workspaces
                .iter()
                .map(|w| PreviewWorkspace {
                    kind: w.kind,
                    path: w.path.clone(),
                    product_version: w.product_version.clone(),
                })
                .collect(),
            projects,
            connection_types,
            issues,
            default_store: self.default_store,
        };
        Ok((preview, payload, existing))
    }

    fn add_root(&self, path: &Path) {
        self.roots.add(path.to_path_buf());
        if let Ok(canonical) = std::fs::canonicalize(path) {
            self.roots.add(canonical);
        }
    }

    fn plan_project(
        &self,
        project: &ninesql_dbeaver::Project,
        import: mirror::ProjectImport,
        existing: &Connections,
        types_after: &ConnectionTypes,
        ids: &BTreeMap<String, ConnectionId>,
        seen_ids: &mut BTreeSet<String>,
    ) -> (ProjectPreview, SecretPayload) {
        let mirror::ProjectImport {
            project: project_name,
            credentials,
            folders,
            connections,
            mut issues,
            ..
        } = import;
        // The canonical source workspace, recorded in `origin` and part of the re-import match.
        let source_workspace =
            std::fs::canonicalize(&project.workspace).unwrap_or_else(|_| project.workspace.clone());

        let mut payload = SecretPayload::new();
        let mut plans = Vec::with_capacity(connections.len());
        for entry in connections {
            let mirror::ConnectionImport {
                dbeaver_id,
                name,
                outcome,
                issues: mut conn_issues,
            } = entry;
            let failed = |reason: String, issues: Vec<ImportIssue>| ConnectionPlan {
                project: project_name.clone(),
                dbeaver_id: dbeaver_id.clone(),
                name: name.clone(),
                status: ImportStatus::Failed,
                action: PlanAction::NotImported { reason },
                connection: None,
                secrets: Vec::new(),
                issues,
            };
            let mut imported = match outcome {
                mirror::Outcome::Imported(c) => c,
                mirror::Outcome::Failed { reason } => {
                    plans.push(failed(reason, conn_issues));
                    continue;
                }
            };
            if !seen_ids.insert(dbeaver_id.clone()) {
                plans.push(failed(
                    "another DBeaver project of this import has a connection with the same id"
                        .to_owned(),
                    conn_issues,
                ));
                continue;
            }
            let secrets = std::mem::take(&mut imported.secrets);
            let previous =
                existing.find_dbeaver_origin(&dbeaver_id, &project_name, &source_workspace);
            if previous.is_none() && existing.find_by_dbeaver_id(&dbeaver_id).is_some() {
                // Same DBeaver id, other project or workspace (or imported before schema 5):
                // never an update of that connection (review, H1).
                conn_issues.push(issue(
                    Severity::Warning,
                    IssueKind::Unsupported,
                    None,
                    "a saved connection has the same DBeaver id but comes from another DBeaver \
                     workspace or project: this one is imported as a new connection",
                ));
            }
            let id = previous
                .map(|c| c.id)
                .or_else(|| ids.get(&dbeaver_id).copied())
                .unwrap_or_else(ConnectionId::new_random);
            let converted = self.convert(
                &imported,
                id,
                &project_name,
                &source_workspace,
                &mut conn_issues,
            );
            let mut connection = match converted {
                Ok(c) => c,
                Err(reason) => {
                    plans.push(failed(reason, conn_issues));
                    continue;
                }
            };
            let mut diff = None;
            if let Some(previous) = previous {
                let imported_slots: Vec<SecretSlot> =
                    secrets.iter().map(|s| core_slot(s.slot)).collect();
                let (merged, d) = merge_update(
                    previous,
                    connection,
                    &imported_slots,
                    types_after,
                    &mut conn_issues,
                );
                connection = merged;
                diff = Some(d);
            }
            // The driver check of an update runs on the merged connection.
            self.check_enabled(&mut connection, &mut conn_issues);
            if connection.validate().is_err() {
                let reason = connection
                    .validate()
                    .err()
                    .map_or_else(String::new, |e| e.to_string());
                plans.push(failed(reason, conn_issues));
                continue;
            }
            if types_after.get(&connection.connection_type).is_none() {
                conn_issues.push(issue(
                    Severity::Warning,
                    IssueKind::Unsupported,
                    Some("configuration.type"),
                    "the connection type is not defined: production rules apply until it is",
                ));
            }

            // Secrets: only fields the converted connection has (prompt fields).
            let mut moves = Vec::new();
            let mut values = Vec::new();
            for s in secrets {
                let slot = core_slot(s.slot);
                match prompt_kind(&connection, slot) {
                    Some(kind) => {
                        moves.push(SecretToMove { slot, kind });
                        values.push((slot, kind, s.value));
                    }
                    None => {
                        conn_issues.push(issue(
                            Severity::Warning,
                            IssueKind::Credentials,
                            None,
                            "a saved secret has no matching credential field and is not imported",
                        ));
                        drop(s.value);
                    }
                }
            }
            if self.default_store.is_none() && !moves.is_empty() {
                conn_issues.push(issue(
                    Severity::Warning,
                    IssueKind::Credentials,
                    None,
                    "no keyring or vault in this session: saved passwords are not imported and are asked at connect",
                ));
            }
            payload.insert((project_name.clone(), dbeaver_id.clone()), values);
            let action = match (previous, diff) {
                (Some(p), Some(diff)) => PlanAction::Update {
                    existing: p.id,
                    diff: Box::new(diff),
                },
                _ => PlanAction::Create,
            };
            let status = if connection.enabled {
                ImportStatus::Imported
            } else {
                ImportStatus::ImportedDisabled
            };
            plans.push(ConnectionPlan {
                project: project_name.clone(),
                dbeaver_id,
                name,
                status,
                action,
                connection: Some(connection),
                secrets: moves,
                issues: conn_issues,
            });
        }

        let used: BTreeSet<Vec<String>> = plans
            .iter()
            .filter_map(|p| p.connection.as_ref())
            .flat_map(|c| (1..=c.folder.len()).map(|n| c.folder[..n].to_vec()))
            .collect();
        let folder_plans: Vec<FolderPlan> = folders
            .into_iter()
            .map(|f| FolderPlan {
                used: used.contains(&f.path),
                path: f.path,
                description: f.description,
            })
            .collect();
        if folder_plans.iter().any(|f| !f.used) {
            issues.push(issue(
                Severity::Info,
                IssueKind::Unsupported,
                Some("folders"),
                "empty folders are not created: NineSQL folders exist through their connections",
            ));
        }
        if folder_plans.iter().any(|f| f.description.is_some()) {
            issues.push(issue(
                Severity::Info,
                IssueKind::Unsupported,
                Some("folders"),
                "folder descriptions are not imported",
            ));
        }
        if plans.iter().any(|p| !p.secrets.is_empty()) {
            issues.push(issue(
                Severity::Info,
                IssueKind::Credentials,
                Some("credentials-config.json"),
                "once you have switched, consider deleting the passwords DBeaver saved: its \
                 credentials file is only obfuscated (NineSQL never changes DBeaver files)",
            ));
        }
        if project.path.join("Scripts").is_dir() {
            issues.push(issue(
                Severity::Info,
                IssueKind::Unsupported,
                Some("Scripts"),
                "SQL scripts are not copied yet (the DBeaver Scripts format is not verified)",
            ));
        }
        (
            ProjectPreview {
                name: project_name,
                path: project.path.clone(),
                dbeaver_workspace: project.workspace.clone(),
                credentials,
                folders: folder_plans,
                connections: plans,
                issues,
            },
            payload,
        )
    }

    /// Mirror → core connection: driver-refused properties dropped, `sanitize()`, the DBeaver
    /// file check, the marker. Disabled reasons from the parser are kept. `Err` with a reason
    /// when a model rule cannot be met (not imported).
    fn convert(
        &self,
        m: &ImportedConnection,
        id: ConnectionId,
        project: &str,
        source_workspace: &Path,
        issues: &mut Vec<ImportIssue>,
    ) -> Result<Connection, String> {
        let engine = EngineId::new(m.engine.as_str()).map_err(|e| e.to_string())?;
        let connection_type =
            ConnectionTypeId::new(m.connection_type.clone()).unwrap_or_else(|_| {
                issues.push(issue(
                    Severity::Warning,
                    IssueKind::InvalidValue,
                    Some("configuration.type"),
                    "invalid connection type id: the Production type is used",
                ));
                ConnectionTypeId::prod()
            });
        let endpoint = match &m.endpoint {
            mirror::Endpoint::Tcp {
                host,
                port,
                database,
            } => Endpoint::Tcp {
                host: host.clone(),
                port: *port,
                database: database.clone(),
            },
            mirror::Endpoint::File { path } => Endpoint::File { path: path.clone() },
        };
        let auth = match &m.auth {
            mirror::Auth::None => Auth::None,
            mirror::Auth::Password { username, .. } => Auth::Password {
                username: username.clone(),
                secret: SecretSource::Prompt,
            },
        };
        let mut c = Connection::new(m.name.clone(), engine, endpoint, auth, connection_type);
        c.id = id;
        c.description.clone_from(&m.description);
        c.ssh = m.ssh.as_ref().map(|t| SshTunnel {
            server: hop(&t.server),
            jump_hosts: t.jump_hosts.iter().map(hop).collect(),
        });
        c.tls = TlsConfig {
            mode: match m.tls.mode {
                mirror::TlsMode::Disable => TlsMode::Disable,
                mirror::TlsMode::Require => TlsMode::Require,
                mirror::TlsMode::VerifyCa => TlsMode::VerifyCa,
                mirror::TlsMode::VerifyFull => TlsMode::VerifyFull,
            },
            ca_file: m.tls.ca_file.clone(),
            client_certificate: m
                .tls
                .client_certificate
                .as_ref()
                .map(|cc| ClientCertificate {
                    cert_file: cc.cert_file.clone(),
                    key_file: cc.key_file.clone(),
                    passphrase: cc.passphrase.map(|_| SecretSource::Prompt),
                }),
        };
        c.read_only = m.read_only;
        c.folder.clone_from(&m.folder);
        c.init_sql.clone_from(&m.init_sql);
        c.default_schema = m.default_schema.clone().filter(|s| !s.trim().is_empty());
        c.default_catalog = m.default_catalog.clone().filter(|s| !s.trim().is_empty());
        c.auto_commit = m.auto_commit;
        c.origin = Origin::Dbeaver {
            dbeaver_id: m.origin.dbeaver_id.clone(),
            project: Some(project.to_owned()),
            source_workspace: Some(source_workspace.to_path_buf()),
        };

        // Driver properties. `ninesql-dbeaver` already converted them to NineSQL names for the
        // engines NineSQL has drivers for, and listed what it dropped (`dropped_properties`,
        // each with a `PropertyDropped` / `ReservedProperty` / `SecretRemoved` / `InvalidValue`
        // report entry). The core still checks every remaining key against the registered
        // driver (defence in depth: a key the driver refuses never reaches it, item 9) and
        // reports only what the crate has not reported already. Without a registered driver the
        // properties are kept and `Connection::try_enable` checks them before enabling.
        let caps = self.registry.capabilities(&c.engine).ok();
        let already_reported = |key: &str| {
            m.dropped_properties
                .iter()
                .any(|d| d.key == key || d.key.ends_with(&format!(".{key}")))
        };
        for (key, value) in &m.properties {
            if let Some(caps) = &caps {
                let refused = if caps.is_reserved_property(key) {
                    Some((
                        IssueKind::ReservedProperty,
                        "reserved by the NineSQL driver of this engine: not imported",
                    ))
                } else if !caps.connect.accepts_property(key) {
                    Some((
                        IssueKind::PropertyDropped,
                        "no equivalent in the NineSQL driver of this engine: not imported",
                    ))
                } else {
                    None
                };
                if let Some((kind, message)) = refused {
                    if !already_reported(key) {
                        issues.push(issue(
                            Severity::Warning,
                            kind,
                            Some(&format!("configuration.properties.{key}")),
                            message,
                        ));
                    }
                    continue;
                }
            }
            c.properties.insert(key.clone(), value.clone());
        }
        for problem in c.sanitize() {
            issues.push(issue(
                Severity::Warning,
                IssueKind::SecretRemoved,
                None,
                &format!("removed: {problem}"),
            ));
        }
        // The parser reports the bootstrap values as `Unsupported` warnings, matched here by key
        // (never by message); the core keeps them, so they become information.
        for i in issues.iter_mut() {
            let kept = match i.key.as_deref() {
                Some("configuration.bootstrap.defaultSchema") => c
                    .default_schema
                    .is_some()
                    .then_some("kept in the connection settings (not applied by the drivers yet)"),
                Some("configuration.bootstrap.defaultCatalog") => c
                    .default_catalog
                    .is_some()
                    .then_some("kept in the connection settings (not applied by the drivers yet)"),
                Some("configuration.bootstrap.autocommit") => c.auto_commit.is_some().then_some(
                    "kept as the connection's auto-commit override (a production type keeps \
                     manual commit)",
                ),
                _ => None,
            };
            if let Some(message) = kept
                && i.kind == IssueKind::Unsupported
            {
                // Kind kept: a default schema / catalog is not applied at connect yet, and the
                // auto-commit override never relaxes a production type.
                i.severity = Severity::Info;
                message.clone_into(&mut i.message);
            }
        }
        if !c.init_sql.is_empty() {
            // NineSQL sets the marker; the DBeaver file never decides it (ADR-0022 §1).
            c.init_sql_needs_confirmation = true;
        }

        // I4: a database file inside a DBeaver workspace opens read-only (DbeaverRoots, with the
        // source workspace registered above).
        if let Endpoint::File { path } = &c.endpoint
            && self.roots.check(path).inside
            && c.read_only != Some(true)
        {
            c.read_only = Some(true);
            issues.push(issue(
                Severity::Warning,
                IssueKind::FileInsideDbeaver,
                Some("configuration.database"),
                "the database file is inside a DBeaver workspace: imported read-only",
            ));
        }

        if !m.enabled {
            let kind = if issues_have(issues, IssueKind::UnsupportedEngine) {
                DisabledKind::NoDriver
            } else {
                DisabledKind::Route
            };
            let message = m
                .disabled_reason
                .clone()
                .unwrap_or_else(|| "disabled by the import".to_owned());
            c.enabled = false;
            c.disabled_reason = Some(DisabledReason::new(kind, message));
        }
        c.validate().map_err(|e| e.to_string())?;
        Ok(c)
    }

    /// An enabled connection that its driver refuses (or that has no driver) is disabled with
    /// the reason.
    fn check_enabled(&self, c: &mut Connection, issues: &mut Vec<ImportIssue>) {
        if !c.enabled {
            return;
        }
        let reason = match self.registry.validate_connection(c) {
            Ok(()) => return,
            Err(e @ CoreError::NoDriver(_)) => {
                DisabledReason::new(DisabledKind::NoDriver, format!("{e} in this build"))
            }
            Err(e) => DisabledReason::new(DisabledKind::Invalid, e.to_string()),
        };
        issues.push(issue(
            Severity::Warning,
            IssueKind::Disabled,
            None,
            &format!("imported disabled: {}", reason.message),
        ));
        c.enabled = false;
        c.disabled_reason = Some(reason);
    }
}

// ---------------------------------------------------------------------------------------------
// Helpers

fn issue(severity: Severity, kind: IssueKind, key: Option<&str>, message: &str) -> ImportIssue {
    ImportIssue {
        severity,
        kind,
        key: key.map(str::to_owned),
        message: message.to_owned(),
    }
}

fn issues_have(issues: &[ImportIssue], kind: IssueKind) -> bool {
    issues.iter().any(|i| i.kind == kind)
}

fn hop(h: &mirror::SshHop) -> SshHop {
    SshHop::new(
        h.host.clone(),
        h.port,
        h.username.clone(),
        match &h.auth {
            mirror::SshAuth::Agent => SshAuth::Agent,
            mirror::SshAuth::Password { .. } => SshAuth::Password {
                secret: SecretSource::Prompt,
            },
            mirror::SshAuth::PublicKey {
                key_path,
                passphrase,
            } => SshAuth::PublicKey {
                key_path: key_path.clone(),
                passphrase: passphrase.map(|s| match s {
                    SecretState::Imported | SecretState::Prompt => SecretSource::Prompt,
                }),
            },
        },
    )
}

const fn core_slot(slot: mirror::SecretSlot) -> SecretSlot {
    match slot {
        mirror::SecretSlot::Auth => SecretSlot::Auth,
        mirror::SecretSlot::SshServer => SecretSlot::SshServer,
        mirror::SecretSlot::SshJump(i) => SecretSlot::SshJump(i),
        mirror::SecretSlot::TlsClientKey => SecretSlot::TlsClientKey,
    }
}

/// The kind of the credential field at `slot` when it is a prompt field (where an imported
/// secret can go), `None` otherwise.
fn prompt_kind(c: &Connection, slot: SecretSlot) -> Option<SecretKind> {
    let prompt = |s: &SecretSource| matches!(s, SecretSource::Prompt);
    let hop_kind = |h: &SshHop| match &h.auth {
        SshAuth::Password { secret } if prompt(secret) => Some(SecretKind::SshPassword),
        SshAuth::PublicKey {
            passphrase: Some(p),
            ..
        } if prompt(p) => Some(SecretKind::SshKeyPassphrase),
        _ => None,
    };
    match slot {
        SecretSlot::Auth => match &c.auth {
            Auth::Password { secret, .. } if prompt(secret) => Some(SecretKind::Password),
            Auth::Token { secret, .. } if prompt(secret) => Some(SecretKind::Token),
            _ => None,
        },
        SecretSlot::SshServer => c.ssh.as_ref().and_then(|t| hop_kind(&t.server)),
        SecretSlot::SshJump(i) => c
            .ssh
            .as_ref()
            .and_then(|t| t.jump_hosts.get(i))
            .and_then(hop_kind),
        SecretSlot::TlsClientKey => c
            .tls
            .client_certificate
            .as_ref()
            .and_then(|cc| cc.passphrase.as_ref())
            .filter(|p| prompt(p))
            .map(|_| SecretKind::ClientKeyPassphrase),
    }
}

/// A fresh reference `<connection id>:<slot>:<uuid>` (like the UI hub): never the reference of
/// a saved secret, so writing it cannot lose an old value (review, M1).
fn fresh_secret_ref(
    store: StoreKind,
    id: ConnectionId,
    slot: SecretSlot,
) -> Result<SecretRef, ImportError> {
    let sid = SecretId::new(format!(
        "{id}:{}:{}",
        slot.id_suffix(),
        uuid::Uuid::new_v4()
    ))
    .map_err(|_| {
        ImportError::Workspace(WorkspaceError::Model(
            crate::error::ConnectionError::InvalidField {
                field: "id".to_owned(),
                reason: "cannot be used to name a secret",
            },
        ))
    })?;
    Ok(SecretRef::new(store, sid))
}

/// Puts the credential field at `slot` back on "ask every time".
fn clear_secret_ref(c: &mut Connection, slot: SecretSlot) {
    fn hop_clear(h: &mut SshHop) {
        match &mut h.auth {
            SshAuth::Password { secret } => *secret = SecretSource::Prompt,
            SshAuth::PublicKey {
                passphrase: Some(p),
                ..
            } => *p = SecretSource::Prompt,
            SshAuth::PublicKey { .. } | SshAuth::Agent => {}
        }
    }
    match slot {
        SecretSlot::Auth => match &mut c.auth {
            Auth::Password { secret, .. } | Auth::Token { secret, .. } => {
                *secret = SecretSource::Prompt;
            }
            Auth::None | Auth::Username { .. } => {}
        },
        SecretSlot::SshServer => {
            if let Some(t) = &mut c.ssh {
                hop_clear(&mut t.server);
            }
        }
        SecretSlot::SshJump(i) => {
            if let Some(h) = c.ssh.as_mut().and_then(|t| t.jump_hosts.get_mut(i)) {
                hop_clear(h);
            }
        }
        SecretSlot::TlsClientKey => {
            if let Some(p) = c
                .tls
                .client_certificate
                .as_mut()
                .and_then(|cc| cc.passphrase.as_mut())
            {
                *p = SecretSource::Prompt;
            }
        }
    }
}

/// Every credential field back on "ask every time".
fn clear_secret_refs(c: &mut Connection) {
    let slots: Vec<SecretSlot> = c.secret_refs().into_iter().map(|(s, _, _)| s).collect();
    for slot in slots {
        clear_secret_ref(c, slot);
    }
}

/// Where a connection's database credentials go: the endpoint without the database name.
fn endpoint_target(c: &Connection) -> Endpoint {
    match &c.endpoint {
        Endpoint::Tcp { host, port, .. } => Endpoint::Tcp {
            host: host.clone(),
            port: *port,
            database: None,
        },
        Endpoint::UnixSocket { path, .. } => Endpoint::UnixSocket {
            path: path.clone(),
            database: None,
        },
        Endpoint::File { path } => Endpoint::File { path: path.clone() },
    }
}

fn hop_label(h: &SshHop) -> String {
    format!("{}@{}:{}", h.username, h.host, h.port)
}

fn ssh_route(c: &Connection) -> Vec<String> {
    c.ssh
        .as_ref()
        .map(|t| t.hops().map(hop_label).collect())
        .unwrap_or_default()
}

/// The private key file of a hop authenticated by key.
fn key_path(h: &SshHop) -> Option<&Path> {
    match &h.auth {
        SshAuth::PublicKey { key_path, .. } => Some(key_path),
        SshAuth::Password { .. } | SshAuth::Agent => None,
    }
}

/// The SSH hops crossed before the hop of `slot` (`user@host:port`, first first): every jump
/// host for the tunnel server, the earlier jump hosts for a jump host.
fn route_before(c: &Connection, slot: SecretSlot) -> Vec<String> {
    let Some(t) = &c.ssh else {
        return Vec::new();
    };
    let n = match slot {
        SecretSlot::SshJump(i) => i.min(t.jump_hosts.len()),
        _ => t.jump_hosts.len(),
    };
    t.jump_hosts[..n].iter().map(hop_label).collect()
}

/// Whether a secret saved for `slot` of `old` may keep being used by `new`: everything it
/// authenticates against is unchanged (review, H1).
/// - database password / token: engine, host, port (or socket / file), user name, TLS mode and
///   CA file, and the SSH route it travels through;
/// - SSH password / key passphrase: the hop's host, port, user name and key file, and the hops
///   in front of it (review L-b, L-c);
/// - TLS client key passphrase: the certificate and key files and the database host.
fn secret_still_applies(old: &Connection, new: &Connection, slot: SecretSlot) -> bool {
    let hop = |c: &Connection| -> Option<SshHop> {
        let t = c.ssh.as_ref()?;
        match slot {
            SecretSlot::SshServer => Some(t.server.clone()),
            SecretSlot::SshJump(i) => t.jump_hosts.get(i).cloned(),
            _ => None,
        }
    };
    match slot {
        SecretSlot::Auth => {
            old.engine == new.engine
                && endpoint_target(old) == endpoint_target(new)
                && old.auth.username() == new.auth.username()
                && old.tls.mode == new.tls.mode
                && old.tls.ca_file == new.tls.ca_file
                && ssh_route(old) == ssh_route(new)
        }
        SecretSlot::SshServer | SecretSlot::SshJump(_) => match (hop(old), hop(new)) {
            (Some(a), Some(b)) => {
                a.host == b.host
                    && a.port == b.port
                    && a.username == b.username
                    && key_path(&a) == key_path(&b)
                    && route_before(old, slot) == route_before(new, slot)
            }
            _ => false,
        },
        SecretSlot::TlsClientKey => {
            let files = |c: &Connection| {
                c.tls
                    .client_certificate
                    .as_ref()
                    .map(|cc| (cc.cert_file.clone(), cc.key_file.clone()))
            };
            files(old).is_some()
                && files(old) == files(new)
                && endpoint_target(old) == endpoint_target(new)
        }
    }
}

/// Whether `a` is at least as strict as `b` on every rule.
fn at_least_as_strict(a: &SafetyPolicy, b: &SafetyPolicy) -> bool {
    (a.production || !b.production)
        && (a.confirm_destructive || !b.confirm_destructive)
        && (a.confirm_every_write || !b.confirm_every_write)
        && (a.read_only || !b.read_only)
        && (!a.auto_commit || b.auto_commit)
        && (!a.log_query_text || b.log_query_text)
}

/// Tightens `base` with the rules of `other`: never weaker on any rule. Name and color stay.
fn tighten(base: &mut ConnectionType, other: &ConnectionType) {
    base.auto_commit &= other.auto_commit;
    base.confirm_destructive |= other.confirm_destructive;
    base.confirm_every_write |= other.confirm_every_write;
    base.read_only |= other.read_only;
    base.log_query_text &= other.log_query_text;
    base.production |= other.production;
}

/// A re-import: the saved connection's user-owned fields win (see [`PlanAction::Update`]).
/// Returns the merged connection and the field-level diff.
fn merge_update(
    old: &Connection,
    mut new: Connection,
    imported_slots: &[SecretSlot],
    types: &ConnectionTypes,
    issues: &mut Vec<ImportIssue>,
) -> (Connection, UpdateDiff) {
    let mut diff = UpdateDiff::default();
    if old.engine != new.engine {
        diff.engine = Some(Change {
            saved: old.engine.clone(),
            imported: new.engine.clone(),
        });
    }
    if old.endpoint != new.endpoint {
        diff.endpoint = Some(Change {
            saved: old.endpoint.clone(),
            imported: new.endpoint.clone(),
        });
    }
    if old.auth.username() != new.auth.username() {
        diff.username = Some(Change {
            saved: old.auth.username().map(str::to_owned),
            imported: new.auth.username().map(str::to_owned),
        });
    }
    if ssh_route(old) != ssh_route(&new) {
        diff.ssh = Some(Change {
            saved: ssh_route(old),
            imported: ssh_route(&new),
        });
    }
    if old.properties != new.properties {
        diff.properties = Some(Change {
            saved: old.properties.clone(),
            imported: new.properties.clone(),
        });
    }
    if old.init_sql != new.init_sql {
        diff.init_sql = Some(Change {
            saved: old.init_sql.clone(),
            imported: new.init_sql.clone(),
        });
    }
    new.id = old.id;
    new.name.clone_from(&old.name);
    new.description.clone_from(&old.description);
    new.folder.clone_from(&old.folder);
    new.password_exchange = old.password_exchange;
    if let (Some(n), Some(o)) = (&mut new.ssh, &old.ssh) {
        // A hop inherits its saved host-key policy only when it is the same hop (user, host,
        // port) behind the same route (the hops in front of it). A non-strict policy is never
        // carried across a route change: the hop stays strict, with a report entry (L-c).
        let old_hops: Vec<SshHop> = o.hops().cloned().collect();
        let new_labels: Vec<String> = n.hops().map(hop_label).collect();
        let old_labels: Vec<String> = old_hops.iter().map(hop_label).collect();
        let mut reset = false;
        for (i, h) in n
            .jump_hosts
            .iter_mut()
            .chain(std::iter::once(&mut n.server))
            .enumerate()
        {
            let same = old_labels.get(..=i) == new_labels.get(..=i);
            match old_hops.get(i) {
                Some(oh) if same => h.host_key_policy = oh.host_key_policy,
                _ => {
                    // The saved hop at this place, or the saved hop with this label anywhere.
                    let prior = old_hops
                        .iter()
                        .find(|oh| hop_label(oh) == new_labels[i])
                        .or_else(|| old_hops.get(i));
                    if prior.is_some_and(|oh| oh.host_key_policy.is_downgrade()) {
                        reset = true;
                    }
                }
            }
        }
        if reset {
            issues.push(issue(
                Severity::Warning,
                IssueKind::SshHostKeyToConfirm,
                Some("configuration.handlers.ssh_tunnel"),
                "the SSH route changed: a saved non-strict host key policy is not kept, the hop \
                 is checked strictly again",
            ));
        }
    }
    if old.read_only == Some(true) {
        new.read_only = Some(true);
    }
    if old.auto_commit == Some(false) {
        new.auto_commit = Some(false);
    }
    // TLS (review, H1 c): a weaker mode or another CA file (a CA file is the only trust anchor,
    // ADR-0019) is not taken without the user's acceptance.
    if old.endpoint.is_network()
        && new.endpoint.is_network()
        && (old.tls.mode != new.tls.mode || old.tls.ca_file != new.tls.ca_file)
    {
        let kept_saved = new.tls.mode < old.tls.mode || new.tls.ca_file != old.tls.ca_file;
        diff.tls = Some(TlsChange {
            saved: (old.tls.mode, old.tls.ca_file.clone()),
            imported: (new.tls.mode, new.tls.ca_file.clone()),
            kept_saved,
        });
        if kept_saved {
            new.tls.mode = new.tls.mode.max(old.tls.mode);
            new.tls.ca_file.clone_from(&old.tls.ca_file);
            issues.push(issue(
                Severity::Warning,
                IssueKind::TlsConverted,
                None,
                "the DBeaver TLS settings are weaker than or differ from the saved ones: the \
                 saved TLS mode and CA file are kept unless you accept the change",
            ));
        }
    }
    if new.connection_type != old.connection_type {
        let imported = types.resolve(&new.connection_type).policy();
        let saved = types.resolve(&old.connection_type).policy();
        let keep = !at_least_as_strict(&imported, &saved);
        diff.connection_type = Some((
            Change {
                saved: old.connection_type.clone(),
                imported: new.connection_type.clone(),
            },
            keep,
        ));
        if keep {
            new.connection_type = old.connection_type.clone();
            issues.push(issue(
                Severity::Warning,
                IssueKind::Unsupported,
                Some("configuration.type"),
                "the DBeaver connection type is less strict than the saved one: the saved type is kept",
            ));
        }
    }
    if !old.enabled && old.disabled_reason.is_none() && new.enabled {
        // Disabled by the user: stays disabled.
        new.enabled = false;
    }
    // Saved secrets: replaced by DBeaver's value, kept while they still apply, else dropped.
    for (slot, kind, reference) in old.secret_refs() {
        let field = prompt_kind(&new, slot) == Some(kind);
        let change = if field && imported_slots.contains(&slot) {
            SecretChangeKind::Replaced
        } else if field && secret_still_applies(old, &new, slot) {
            let _ = new.set_secret_ref(slot, reference.clone());
            SecretChangeKind::Kept
        } else {
            issues.push(issue(
                Severity::Warning,
                IssueKind::Credentials,
                None,
                "a secret saved in NineSQL is not kept: what it authenticates against changed \
                 (it is deleted after the import; NineSQL asks for it at connect)",
            ));
            SecretChangeKind::Dropped
        };
        diff.secrets.push(SecretChange { slot, change });
    }
    // Init SQL is marked again, even if the saved copy was confirmed (ADR-0022 §1).
    new.init_sql_needs_confirmation = !new.init_sql.is_empty();
    (new, diff)
}

/// DBeaver types of every project → actions against the project's types. A type id defined
/// more than once is folded tighten-only (review, M2); the result only ever tightens an
/// existing type, the built-in ones included (review, M3), each reported.
fn plan_types(
    imported: &[&ImportedConnectionType],
    current: &ConnectionTypes,
    issues: &mut Vec<ImportIssue>,
) -> Vec<TypePlan> {
    let mut folded: BTreeMap<ConnectionTypeId, (String, String, ConnectionType)> = BTreeMap::new();
    for t in imported {
        let Ok(id) = ConnectionTypeId::new(t.id.clone()) else {
            issues.push(issue(
                Severity::Warning,
                IssueKind::InvalidValue,
                Some(&format!("connection-types.{}", t.dbeaver_id)),
                "invalid connection type id: not imported",
            ));
            continue;
        };
        let wanted = ConnectionType {
            id: id.clone(),
            name: t.name.clone(),
            color: Color::rgb(t.color.r, t.color.g, t.color.b),
            auto_commit: t.auto_commit,
            confirm_destructive: t.confirm_destructive,
            confirm_every_write: t.confirm_every_write,
            read_only: t.read_only,
            log_query_text: t.log_query_text,
            production: t.production,
        };
        match folded.get_mut(&id) {
            Some((_, _, have)) => tighten(have, &wanted),
            None => {
                folded.insert(id, (t.dbeaver_id.clone(), t.name.clone(), wanted));
            }
        }
    }
    let builtins: Vec<ConnectionTypeId> = ConnectionType::builtins()
        .into_iter()
        .map(|t| t.id)
        .collect();
    let mut out = Vec::new();
    for (id, (dbeaver_id, name, wanted)) in folded {
        let action = match current.get(&id) {
            Some(have) => {
                let mut merged = have.clone();
                tighten(&mut merged, &wanted);
                if merged == *have {
                    TypeAction::Keep
                } else {
                    issues.push(issue(
                        Severity::Warning,
                        IssueKind::Unsupported,
                        Some(&format!("connection-types.{dbeaver_id}")),
                        if builtins.contains(&id) {
                            "the NineSQL built-in type of the same id is made stricter to keep \
                             DBeaver's rules (confirmations, manual commit); this applies to \
                             every connection of that type"
                        } else {
                            "the existing type of the same id is made stricter to keep DBeaver's \
                             rules; this applies to every connection of that type"
                        },
                    ));
                    TypeAction::Tighten(merged)
                }
            }
            None => TypeAction::Create(wanted),
        };
        out.push(TypePlan {
            dbeaver_id,
            id,
            name,
            action,
        });
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Report file

/// `imports/dbeaver-<timestamp>.json`: the import report saved in the project (ADR-0006 §6).
/// Names, ids, statuses, counts and report entries; never a secret value, host, user name or
/// property value from the source beyond what the connection itself saves (I1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportReportFile {
    /// Seconds since the Unix epoch.
    pub created_at: u64,
    /// DBeaver workspaces read.
    pub sources: Vec<PathBuf>,
    /// Store the secrets went to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store: Option<StoreKind>,
    /// Entries not tied to one project (connection types).
    #[serde(default)]
    pub issues: Vec<ReportIssue>,
    /// Per project.
    pub projects: Vec<ReportProject>,
}

impl Document for ImportReportFile {
    const NAME: &'static str = "import-report";
    const FORMAT: Format = Format::Json;
    const SCHEMA_VERSION: u32 = 1;
}

/// One project of the report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportProject {
    /// DBeaver project name.
    pub name: String,
    /// What happened to DBeaver's `credentials-config.json` (the key avoids the secret-like
    /// fragments the storage layer strips on load).
    pub saved_logins_file: String,
    /// Project-level entries.
    pub issues: Vec<ReportIssue>,
    /// Connections.
    pub connections: Vec<ReportConnection>,
}

impl ReportProject {
    fn new(p: &ProjectPreview, connections: Vec<ReportConnection>) -> Self {
        Self {
            name: p.name.clone(),
            saved_logins_file: match &p.credentials {
                CredentialsStatus::NotFound => "not-found".to_owned(),
                CredentialsStatus::Decrypted { passwords } => {
                    format!("decrypted ({passwords} saved)")
                }
                CredentialsStatus::ReadFailed => "read-failed".to_owned(),
                CredentialsStatus::Unreadable(e) => format!("unreadable: {e}"),
            },
            issues: p.issues.iter().map(ReportIssue::from).collect(),
            connections,
        }
    }
}

/// One connection of the report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportConnection {
    /// DBeaver id.
    pub dbeaver_id: String,
    /// Name.
    pub name: String,
    /// Status (`skipped` when the user left it out).
    pub status: String,
    /// NineSQL id, when written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection: Option<ConnectionId>,
    /// `create`, `update`, `not-imported`, `skipped`.
    pub action: String,
    /// Why it is disabled or not imported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Saved credentials moved to the store (a count, never values). The key name avoids the
    /// secret-like fragments the storage layer strips on load.
    pub moved_to_store: usize,
    /// Entries.
    pub issues: Vec<ReportIssue>,
}

impl ReportConnection {
    fn from_plan(plan: &ConnectionPlan, written: Option<&Connection>, secrets: usize) -> Self {
        let (action, reason) = match &plan.action {
            PlanAction::Create => ("create", None),
            PlanAction::Update { .. } => ("update", None),
            PlanAction::NotImported { reason } => ("not-imported", Some(reason.clone())),
        };
        Self {
            dbeaver_id: plan.dbeaver_id.clone(),
            name: plan.name.clone(),
            status: match plan.status {
                ImportStatus::Imported => "imported",
                ImportStatus::ImportedDisabled => "imported-disabled",
                ImportStatus::Failed => "failed",
            }
            .to_owned(),
            connection: written.map(|c| c.id),
            action: action.to_owned(),
            reason: reason.or_else(|| {
                written
                    .and_then(|c| c.disabled_reason.as_ref())
                    .map(|r| r.message.clone())
            }),
            moved_to_store: secrets,
            issues: plan.issues.iter().map(ReportIssue::from).collect(),
        }
    }

    fn skipped(plan: &ConnectionPlan) -> Self {
        let mut r = Self::from_plan(plan, None, 0);
        r.status = "skipped".to_owned();
        r.action = "skipped".to_owned();
        r
    }
}

/// One report entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportIssue {
    /// `info`, `warning`, `error`.
    pub severity: String,
    /// Category.
    pub kind: String,
    /// DBeaver key concerned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// Explanation.
    pub message: String,
}

impl From<&ImportIssue> for ReportIssue {
    fn from(i: &ImportIssue) -> Self {
        Self {
            severity: match i.severity {
                Severity::Info => "info",
                Severity::Warning => "warning",
                Severity::Error => "error",
            }
            .to_owned(),
            kind: format!("{:?}", i.kind),
            key: i.key.clone(),
            message: i.message.clone(),
        }
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// `YYYYMMDDTHHMMSSZ` (UTC) for Unix seconds.
fn timestamp(secs: u64) -> String {
    let days = i64::try_from(secs / 86_400).unwrap_or(0);
    let rem = secs % 86_400;
    // Howard Hinnant's civil-from-days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

impl fmt::Display for ImportStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Imported => "imported",
            Self::ImportedDisabled => "imported disabled",
            Self::Failed => "not imported",
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).
mod tests {
    use super::*;

    #[test]
    fn timestamps_are_utc_civil_dates() {
        assert_eq!(timestamp(0), "19700101T000000Z");
        assert_eq!(timestamp(951_782_400), "20000229T000000Z");
        assert_eq!(timestamp(1_794_787_199), "20261115T235959Z");
    }

    #[test]
    fn strictness_compares_every_rule() {
        let dev = ConnectionType::development().policy();
        let prod = ConnectionType::production().policy();
        assert!(at_least_as_strict(&prod, &dev));
        assert!(!at_least_as_strict(&dev, &prod));
        assert!(at_least_as_strict(&dev, &dev));
        let mut ro = dev;
        ro.read_only = true;
        assert!(!at_least_as_strict(&dev, &ro));
    }

    #[test]
    fn types_never_overwrite_builtins_and_only_tighten() {
        let mk =
            |id: &str, builtin: bool, auto_commit: bool, confirm: bool| ImportedConnectionType {
                id: id.to_owned(),
                dbeaver_id: id.to_owned(),
                builtin,
                name: id.to_owned(),
                color: mirror::Rgb { r: 1, g: 2, b: 3 },
                auto_commit,
                confirm_destructive: confirm,
                confirm_every_write: false,
                read_only: false,
                log_query_text: true,
                production: false,
            };
        let by_id = |plans: &[TypePlan], id: &str| {
            plans
                .iter()
                .find(|p| p.id.as_str() == id)
                .unwrap()
                .action
                .clone()
        };
        let mut staging = ConnectionType::development();
        staging.id = ConnectionTypeId::new("staging").unwrap();
        staging.auto_commit = false;
        staging.confirm_destructive = true;
        let current = ConnectionTypes::from_saved(vec![staging.clone()]);
        let mut issues = Vec::new();
        let (prod, dev, weak_staging, qa, test) = (
            // A crafted `prod` that would relax the built-in: never weaker.
            mk("prod", true, true, false),
            // Not flagged built-in, same id: the built-in, untouched (nothing stricter).
            mk("dev", false, true, false),
            // Weaker than the saved one: kept.
            mk("staging", false, true, false),
            mk("qa", false, true, true),
            // DBeaver's `test` confirms data changes: the built-in is tightened (review, M3).
            mk("test", true, true, true),
        );
        let plans = plan_types(
            &[&prod, &dev, &weak_staging, &qa, &test],
            &current,
            &mut issues,
        );
        assert_eq!(by_id(&plans, "prod"), TypeAction::Keep);
        assert_eq!(by_id(&plans, "dev"), TypeAction::Keep);
        assert_eq!(by_id(&plans, "staging"), TypeAction::Keep);
        assert!(matches!(by_id(&plans, "qa"), TypeAction::Create(t) if t.confirm_destructive));
        match by_id(&plans, "test") {
            TypeAction::Tighten(t) => {
                assert!(t.confirm_destructive);
                assert_eq!(t.name, "Test", "name and color stay");
            }
            other => panic!("{other:?}"),
        }
        assert!(issues.iter().any(|i| i.message.contains("built-in")));

        let mut relaxed = staging;
        relaxed.confirm_destructive = false;
        relaxed.auto_commit = true;
        let current = ConnectionTypes::from_saved(vec![relaxed]);
        let strict = mk("staging", false, false, true);
        let plans = plan_types(&[&strict], &current, &mut issues);
        match &plans[0].action {
            TypeAction::Tighten(t) => {
                assert!(t.confirm_destructive);
                assert!(!t.auto_commit);
            }
            other => panic!("{other:?}"),
        }
    }

    /// Review M2: the same new type id in two projects folds tighten-only, in either order.
    #[test]
    fn a_type_defined_twice_keeps_the_stricter_rules() {
        let mk = |confirm: bool, auto_commit: bool| ImportedConnectionType {
            id: "qa".to_owned(),
            dbeaver_id: "qa".to_owned(),
            builtin: false,
            name: "QA".to_owned(),
            color: mirror::Rgb { r: 1, g: 2, b: 3 },
            auto_commit,
            confirm_destructive: confirm,
            confirm_every_write: false,
            read_only: false,
            log_query_text: true,
            production: false,
        };
        let (strict, weak) = (mk(true, false), mk(false, true));
        for order in [[&strict, &weak], [&weak, &strict]] {
            let plans = plan_types(&order, &ConnectionTypes::default(), &mut Vec::new());
            assert_eq!(plans.len(), 1);
            match &plans[0].action {
                TypeAction::Create(t) => assert!(t.confirm_destructive && !t.auto_commit),
                other => panic!("{other:?}"),
            }
        }
    }
}
