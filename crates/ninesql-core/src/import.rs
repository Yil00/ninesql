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
//!    dropped), merges the connection types (never overwriting a built-in, never weakening an
//!    existing type), writes the connections in one checked bulk write that keeps the imported
//!    init SQL marker ([`Workspace`]), and saves the import report in the project
//!    (`imports/dbeaver-<timestamp>.json`, no secret).
//!
//! Both are **blocking** (file system, secret stores). The application calls them through
//! [`crate::CoreServices::preview_import`] and [`crate::CoreServices::apply_import`], which run
//! them on the driver runtime's blocking pool and answer through a [`crate::Reply`] (I6).
//!
//! Re-import is idempotent (ADR-0006 §8, F-CON-14): a connection whose `origin.dbeaver_id` is
//! already saved is updated in place (same NineSQL id) instead of duplicated. What the update
//! keeps from the saved connection is listed on [`PlanAction::Update`].
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
    /// Updates the connection already imported from the same DBeaver id (F-CON-14). Kept from
    /// the saved connection: its id, name, description, folder, password-exchange policy,
    /// per-hop SSH host key policies (same host and port), a stricter read-only or manual-commit
    /// override, a stricter connection type (the imported type is taken only when its rules
    /// are at least as strict on every point), a "disabled by the user" state, and saved secrets
    /// of fields DBeaver has no saved value for. Taken from DBeaver: the endpoint, engine,
    /// user names, SSH hops, TLS, driver properties, init SQL (marked again as needing
    /// confirmation), default schema / catalog and auto-commit.
    Update {
        /// The saved connection.
        existing: ConnectionId,
    },
    /// Not imported (the parser failed it, or its converted settings break a model rule).
    NotImported {
        /// Why (no value from the file).
        reason: String,
    },
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
    /// A NineSQL built-in type (`dev`, `test`, `prod`): its connections use the built-in as it
    /// is in this project; the DBeaver settings never overwrite it.
    Builtin,
    /// A new user-defined type.
    Create(ConnectionType),
    /// A type of the same id exists and the DBeaver one is stricter on some point: the existing
    /// type is made stricter there (never weaker).
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
    /// Connection types.
    pub connection_types: Vec<TypePlan>,
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
    /// Where secrets go by default (the session's backend); `None`: no store, saved passwords
    /// are not imported and NineSQL asks for them at connect.
    pub default_store: Option<StoreKind>,
}

impl ImportPreview {
    /// Every connection plan, all projects together.
    pub fn connections(&self) -> impl Iterator<Item = &ConnectionPlan> {
        self.projects.iter().flat_map(|p| p.connections.iter())
    }

    /// Number of secrets the import would move.
    #[must_use]
    pub fn secret_count(&self) -> usize {
        self.connections().map(|c| c.secrets.len()).sum()
    }

    /// The connection to use for a test-before-save of the plan of `dbeaver_id`: the converted
    /// connection, with the imported init SQL marker set when it has init SQL (ADR-0022 §1, the
    /// connection is not on disk yet, so the core uses the request's marker). `None` when the
    /// connection is not imported or is disabled. Its saved credentials are not in a store yet:
    /// the open asks for them ([`crate::OpenOutcome::NeedsSecrets`]).
    #[must_use]
    pub fn connection_for_test(&self, dbeaver_id: &str) -> Option<Connection> {
        let c = self
            .connections()
            .find(|c| c.dbeaver_id == dbeaver_id)?
            .connection
            .clone()?;
        c.enabled.then_some(c)
    }

    /// `current` with the type changes of this preview applied (what
    /// [`Importer::apply`] would save), for the policy of a test-before-save.
    #[must_use]
    pub fn types_after(&self, current: &ConnectionTypes) -> ConnectionTypes {
        let mut types = current.clone();
        for t in self.projects.iter().flat_map(|p| p.connection_types.iter()) {
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
}

impl ImportChoices {
    /// Everything, secrets to `store`, existing connections updated.
    #[must_use]
    pub fn all(store: Option<StoreKind>) -> Self {
        Self {
            store,
            skip: BTreeSet::new(),
            update_existing: true,
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
    /// The import report saved in the project.
    pub report_file: PathBuf,
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
    /// The blocking task failed.
    #[error("import task: {0}")]
    Runtime(String),
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
        let (preview, payload) = self.plan(source, &BTreeMap::new())?;
        drop(payload);
        Ok(preview)
    }

    /// Applies a preview with the user's choices. **Blocking.** See the module documentation.
    ///
    /// # Errors
    /// [`ImportError::SourceChanged`] (preview again), [`ImportError::VaultLocked`],
    /// [`ImportError::NoStore`], [`ImportError::Secret`], [`ImportError::Workspace`], and the
    /// source errors of [`Self::preview`]. Secrets stored before a failure are deleted again
    /// (best effort; a value that replaced an earlier one at the same reference cannot be
    /// restored).
    pub fn apply(
        &self,
        preview: &ImportPreview,
        choices: &ImportChoices,
    ) -> Result<ImportOutcome, ImportError> {
        let ids: BTreeMap<String, ConnectionId> = preview
            .connections()
            .filter_map(|c| Some((c.dbeaver_id.clone(), c.connection.as_ref()?.id)))
            .collect();
        let (fresh, mut payload) = self.plan(&preview.source, &ids)?;
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

        // Connection types first: a connection never names a type that is not saved.
        let mut types = self.workspace.load_connection_types()?;
        let mut types_changed = 0;
        for t in fresh
            .projects
            .iter()
            .flat_map(|p| p.connection_types.iter())
        {
            if let TypeAction::Create(t) | TypeAction::Tighten(t) = &t.action {
                types.upsert(t.clone());
                types_changed += 1;
            }
        }
        if types_changed > 0 {
            self.workspace.save_connection_types(&types)?;
        }

        let existing = self.workspace.load_connections()?.connections;
        let mut stored: Vec<SecretRef> = Vec::new();
        let mut upserts = Vec::new();
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
                    let mut count = 0;
                    if let Some(store) = choices.store {
                        for (slot, kind, value) in secrets {
                            let reference = secret_ref(store, connection.id, slot)?;
                            let meta = SecretMeta {
                                connection_id: connection.id.to_string(),
                                kind,
                                label: format!("NineSQL: {}", connection.name),
                            };
                            self.stores.save(&reference, &meta, &value)?;
                            drop(value);
                            let previous = existing.get(connection.id).is_some_and(|e| {
                                e.secret_refs().iter().any(|(_, _, r)| **r == reference)
                            });
                            if !previous {
                                stored.push(reference.clone());
                            }
                            connection
                                .set_secret_ref(slot, reference)
                                .map_err(WorkspaceError::from)?;
                            count += 1;
                        }
                    } else {
                        // No store chosen: the values are dropped (zeroized), the fields ask.
                        drop(secrets);
                    }
                    moved += count;
                    match plan.action {
                        PlanAction::Update { .. } => updated += 1,
                        _ => created += 1,
                    }
                    if !connection.enabled {
                        disabled += 1;
                    }
                    report_connections.push(ReportConnection::from_plan(
                        plan,
                        Some(&connection),
                        count,
                    ));
                    upserts.push(connection);
                }
                entries.push(ReportProject::new(project, report_connections));
            }
            let written = self
                .workspace
                .save_imported(upserts.clone(), &self.registry)?;
            Ok::<_, ImportError>(written)
        })();
        let written = match result {
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

        // References of updated connections that the new version no longer uses.
        for c in &upserts {
            let Some(old) = existing.get(c.id) else {
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
            projects: entries,
        };
        let report_file = self.save_report(&report)?;
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
            connections: written,
        })
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
    /// side. `ids` fixes the NineSQL id of new connections (from an earlier preview).
    fn plan(
        &self,
        source: &ImportSource,
        ids: &BTreeMap<String, ConnectionId>,
    ) -> Result<(ImportPreview, SecretPayload), ImportError> {
        let workspaces = match source {
            ImportSource::Detect(roots) => {
                let found = ninesql_dbeaver::detect_workspaces(roots);
                if found.is_empty() {
                    return Err(ImportError::NothingFound);
                }
                found
            }
            ImportSource::Folder(path) => vec![ninesql_dbeaver::open_folder(path)?],
        };
        for w in &workspaces {
            self.add_root(&w.path);
            for p in &w.projects {
                self.add_root(&p.workspace);
            }
        }
        let existing = self.workspace.load_connections()?.connections;
        let current_types = self.workspace.load_connection_types()?;

        let mut payload = SecretPayload::new();
        let mut projects = Vec::new();
        let mut seen_ids = BTreeSet::new();
        for w in &workspaces {
            for project in &w.projects {
                let import = ninesql_dbeaver::import_project(project);
                let (preview, secrets) = self.plan_project(
                    project,
                    import,
                    &existing,
                    &current_types,
                    ids,
                    &mut seen_ids,
                );
                payload.extend(secrets);
                projects.push(preview);
            }
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
            default_store: self.default_store,
        };
        Ok((preview, payload))
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
        current_types: &ConnectionTypes,
        ids: &BTreeMap<String, ConnectionId>,
        seen_ids: &mut BTreeSet<String>,
    ) -> (ProjectPreview, SecretPayload) {
        let mirror::ProjectImport {
            project: project_name,
            credentials,
            folders,
            connection_types,
            connections,
            mut issues,
            ..
        } = import;

        let type_plans = plan_types(&connection_types, current_types, &mut issues);
        let mut types_after = current_types.clone();
        for t in &type_plans {
            if let TypeAction::Create(t) | TypeAction::Tighten(t) = &t.action {
                types_after.upsert(t.clone());
            }
        }

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
            let previous = existing.find_by_dbeaver_id(&dbeaver_id);
            let id = previous
                .map(|c| c.id)
                .or_else(|| ids.get(&dbeaver_id).copied())
                .unwrap_or_else(ConnectionId::new_random);
            let converted = self.convert(&imported, id, &project_name, &mut conn_issues);
            let mut connection = match converted {
                Ok(c) => c,
                Err(reason) => {
                    plans.push(failed(reason, conn_issues));
                    continue;
                }
            };
            if let Some(previous) = previous {
                let imported_slots: Vec<SecretSlot> =
                    secrets.iter().map(|s| core_slot(s.slot)).collect();
                connection = merge_update(
                    previous,
                    connection,
                    &imported_slots,
                    &types_after,
                    &mut conn_issues,
                );
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
            let action = match previous {
                Some(p) => PlanAction::Update { existing: p.id },
                None => PlanAction::Create,
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
                connection_types: type_plans,
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
        };

        // Driver properties: only what the engine's driver accepts reaches the connection
        // (item 9: pgjdbc / Connector/J properties without an equivalent are refused at connect).
        let caps = self.registry.capabilities(&c.engine).ok();
        for (key, value) in &m.properties {
            if let Some(caps) = &caps {
                if caps.is_reserved_property(key) {
                    issues.push(issue(
                        Severity::Warning,
                        IssueKind::ReservedProperty,
                        Some(&format!("configuration.properties.{key}")),
                        "reserved by the NineSQL driver of this engine: not imported",
                    ));
                    continue;
                }
                if !caps.connect.accepts_property(key) {
                    issues.push(issue(
                        Severity::Warning,
                        IssueKind::Unsupported,
                        Some(&format!("configuration.properties.{key}")),
                        "no equivalent in the NineSQL driver of this engine: not imported",
                    ));
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
        // The parser reports the bootstrap values it could not hand over; the core keeps them.
        for i in issues.iter_mut() {
            let kept = match i.key.as_deref() {
                Some("configuration.bootstrap.defaultSchema") => c.default_schema.is_some(),
                Some("configuration.bootstrap.defaultCatalog") => c.default_catalog.is_some(),
                Some("configuration.bootstrap.autocommit") => c.auto_commit.is_some(),
                _ => false,
            };
            if kept && i.kind == IssueKind::Unsupported {
                // Still `Unsupported` at connect: no driver applies it yet.
                i.severity = Severity::Info;
                "kept in the connection settings (not applied by the drivers yet)"
                    .clone_into(&mut i.message);
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

fn secret_ref(
    store: StoreKind,
    id: ConnectionId,
    slot: SecretSlot,
) -> Result<SecretRef, ImportError> {
    let sid = SecretId::new(format!("{id}:{}", slot.id_suffix())).map_err(|_| {
        ImportError::Workspace(WorkspaceError::Model(
            crate::error::ConnectionError::InvalidField {
                field: "id".to_owned(),
                reason: "cannot be used to name a secret",
            },
        ))
    })?;
    Ok(SecretRef::new(store, sid))
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

/// A re-import: the saved connection's user-owned fields win (see [`PlanAction::Update`]).
fn merge_update(
    old: &Connection,
    mut new: Connection,
    imported_slots: &[SecretSlot],
    types: &ConnectionTypes,
    issues: &mut Vec<ImportIssue>,
) -> Connection {
    new.id = old.id;
    new.name.clone_from(&old.name);
    new.description.clone_from(&old.description);
    new.folder.clone_from(&old.folder);
    new.password_exchange = old.password_exchange;
    if let (Some(n), Some(o)) = (&mut new.ssh, &old.ssh) {
        let old_hops: Vec<&SshHop> = o.hops().collect();
        let count = n.jump_hosts.len() + 1;
        for (i, h) in n
            .jump_hosts
            .iter_mut()
            .chain(std::iter::once(&mut n.server))
            .enumerate()
        {
            // Same position counted from the server, same host and port.
            let offset = count - i;
            if let Some(oh) = old_hops
                .len()
                .checked_sub(offset)
                .and_then(|j| old_hops.get(j))
                && oh.host == h.host
                && oh.port == h.port
            {
                h.host_key_policy = oh.host_key_policy;
            }
        }
    }
    if old.read_only == Some(true) {
        new.read_only = Some(true);
    }
    if old.auto_commit == Some(false) {
        new.auto_commit = Some(false);
    }
    if new.connection_type != old.connection_type {
        let imported = types.resolve(&new.connection_type).policy();
        let saved = types.resolve(&old.connection_type).policy();
        if !at_least_as_strict(&imported, &saved) {
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
    // Saved secrets of fields DBeaver has no value for (typed in NineSQL) stay.
    for (slot, kind, reference) in old.secret_refs() {
        if !imported_slots.contains(&slot) && prompt_kind(&new, slot) == Some(kind) {
            let _ = new.set_secret_ref(slot, reference.clone());
        }
    }
    // Init SQL is marked again, even if the saved copy was confirmed (ADR-0022 §1).
    new.init_sql_needs_confirmation = !new.init_sql.is_empty();
    new
}

/// DBeaver types → actions against the project's types.
fn plan_types(
    imported: &[ImportedConnectionType],
    current: &ConnectionTypes,
    issues: &mut Vec<ImportIssue>,
) -> Vec<TypePlan> {
    let builtins: Vec<ConnectionTypeId> = ConnectionType::builtins()
        .into_iter()
        .map(|t| t.id)
        .collect();
    let mut out = Vec::new();
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
        let action = if t.builtin || builtins.contains(&id) {
            TypeAction::Builtin
        } else if let Some(have) = current.get(&id) {
            let mut merged = have.clone();
            merged.auto_commit &= wanted.auto_commit;
            merged.confirm_destructive |= wanted.confirm_destructive;
            merged.confirm_every_write |= wanted.confirm_every_write;
            merged.read_only |= wanted.read_only;
            merged.log_query_text &= wanted.log_query_text;
            merged.production |= wanted.production;
            if merged == *have {
                TypeAction::Keep
            } else {
                TypeAction::Tighten(merged)
            }
        } else {
            TypeAction::Create(wanted)
        };
        out.push(TypePlan {
            dbeaver_id: t.dbeaver_id.clone(),
            id,
            name: t.name.clone(),
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
        let mut staging = ConnectionType::development();
        staging.id = ConnectionTypeId::new("staging").unwrap();
        staging.auto_commit = false;
        staging.confirm_destructive = true;
        let current = ConnectionTypes::from_saved(vec![staging.clone()]);
        let mut issues = Vec::new();
        let plans = plan_types(
            &[
                // A crafted `prod` that would relax the built-in: never applied.
                mk("prod", true, true, false),
                // Not flagged built-in but the same id: still the built-in.
                mk("dev", false, true, false),
                // Weaker than the saved one: kept.
                mk("staging", false, true, false),
                mk("qa", false, true, true),
            ],
            &current,
            &mut issues,
        );
        assert_eq!(plans[0].action, TypeAction::Builtin);
        assert_eq!(plans[1].action, TypeAction::Builtin);
        assert_eq!(plans[2].action, TypeAction::Keep);
        assert!(matches!(&plans[3].action, TypeAction::Create(t) if t.confirm_destructive));

        let mut relaxed = staging;
        relaxed.confirm_destructive = false;
        relaxed.auto_commit = true;
        let current = ConnectionTypes::from_saved(vec![relaxed]);
        let plans = plan_types(&[mk("staging", false, false, true)], &current, &mut issues);
        match &plans[0].action {
            TypeAction::Tighten(t) => {
                assert!(t.confirm_destructive);
                assert!(!t.auto_commit);
            }
            other => panic!("{other:?}"),
        }
    }
}
