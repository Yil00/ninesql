//! Workspace files of a project and the connection flows that touch secrets (ADR-0008, I1).
//!
//! On-disk documents:
//! - `project.json`: [`ProjectFile`] (`schema_version` 2)
//! - `connections.json`: [`ConnectionsFile`] (`schema_version` 5)
//! - `connection-types.json`: [`ConnectionTypesFile`] (`schema_version` 1)
//!
//! Secrets given to [`Workspace::create_connection`] go to the secret store; only their
//! [`SecretRef`] reaches `connections.json`.
//!
//! An open [`Workspace`] holds the advisory workspace lock ([`ninesql_storage::lock`]): a second
//! NineSQL process gets [`ninesql_storage::StorageError::Locked`] instead of writing the same
//! files at the same time.

use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use ninesql_secrets::{SecretId, SecretMeta, SecretRef, SecretStores, SecretString, StoreKind};
use ninesql_storage::document::{self, Document, Format, Migration};
use ninesql_storage::fs::ensure_private_dir;
use ninesql_storage::hygiene::zeroize_value;
use ninesql_storage::{DirLock, Paths, ProjectName, ProjectPaths};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::connection::{Connection, ConnectionId, SecretSlot};
use crate::connection_type::{ConnectionType, ConnectionTypes};
use crate::error::{ConnectionError, CoreError, WorkspaceError};
use crate::registry::DriverRegistry;

/// Stable identifier of a project (UUID v4), stored in `project.json`.
///
/// Unlike the project name, it does not change when the project is renamed. It is the value of
/// the `project` attribute of the project's Secret Service items (review item N6), so renaming a
/// project keeps its saved secrets.
///
/// Copying a project directory copies its `project.json` and so its identifier: the copy then
/// shares the original's keyring items (same project id, same connection ids). Give the copy a
/// new identifier ([`ProjectId::new_random`]) when duplicating a project.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProjectId(Uuid);

impl ProjectId {
    /// A new random identifier.
    #[must_use]
    pub fn new_random() -> Self {
        Self(Uuid::new_v4())
    }

    /// Wraps an existing UUID.
    #[must_use]
    pub const fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }

    /// The UUID.
    #[must_use]
    pub const fn as_uuid(&self) -> &Uuid {
        &self.0
    }
}

impl fmt::Display for ProjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.hyphenated().fmt(f)
    }
}

/// `project.json`.
///
/// Schema history: version 1 had `name` and `description`; version 2 adds `id` (fixture
/// `tests/fixtures/project/v1.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectFile {
    /// Stable project identifier (since schema version 2).
    pub id: ProjectId,
    /// Project name (same as its directory name).
    pub name: ProjectName,
    /// Free text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// `project.json` 1 → 2: give the project a stable random identifier.
fn project_v1_to_v2(doc: &mut Value) -> Result<(), String> {
    let obj = doc
        .as_object_mut()
        .ok_or_else(|| "the project file must be an object".to_owned())?;
    obj.insert(
        "id".to_owned(),
        Value::String(ProjectId::new_random().to_string()),
    );
    Ok(())
}

const PROJECT_MIGRATIONS: &[Migration] = &[Migration {
    from: 1,
    description: "add a stable project id",
    apply: project_v1_to_v2,
}];

impl Document for ProjectFile {
    const NAME: &'static str = "project";
    const FORMAT: Format = Format::Json;
    const SCHEMA_VERSION: u32 = 2;
    fn migrations() -> &'static [Migration] {
        PROJECT_MIGRATIONS
    }
}

/// `connections.json`: the saved connections of a project, without secrets.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionsFile {
    /// Connections, in display order.
    #[serde(default)]
    pub connections: Vec<Connection>,
}

impl Document for ConnectionsFile {
    const NAME: &'static str = "connections";
    const FORMAT: Format = Format::Json;
    /// Schema history: version 1 is the first published schema (fixture
    /// `tests/fixtures/connections/v1.json`); version 2 adds the optional per-hop
    /// `host_key_policy` of SSH hops (ADR-0016), strict when absent; version 3 adds the optional
    /// `password_exchange` of a connection (`any` when absent, `scram-only`); version 4 adds the
    /// optional `init_sql_needs_confirmation` marker of imported init SQL (`false` when
    /// absent); version 5 adds `default_schema`, `default_catalog`, the per-connection
    /// `auto_commit` override and the `disabled_reason` of a connection (P2-C, all absent by
    /// default) and the `source_workspace` of a DBeaver origin. A bump adds a
    /// [`Migration`] here. An older NineSQL refuses a newer file instead of dropping a field it
    /// does not know and saving the connection weaker than the user set it.
    const SCHEMA_VERSION: u32 = 5;
    fn migrations() -> &'static [Migration] {
        CONNECTIONS_MIGRATIONS
    }

    /// Applies the rules of [`Connection::sanitize`] to the raw file (any schema version): every
    /// driver property that is secret-like, reserved or carries a credential, and every init SQL
    /// statement with a credential, is removed and zeroized. Non-string property values and init
    /// SQL entries are removed too (they would not load anyway). Unknown layouts are left alone:
    /// secret-looking keys are stripped by the storage layer in any case.
    fn scrub(value: &mut Value, _schema_version: u32) {
        let Some(connections) = value.get_mut("connections").and_then(Value::as_array_mut) else {
            return;
        };
        for connection in connections {
            if let Some(props) = connection
                .get_mut("properties")
                .and_then(Value::as_object_mut)
            {
                props.retain(|key, v| {
                    let keep = v
                        .as_str()
                        .is_some_and(|s| crate::connection::property_problem(key, s).is_none());
                    if !keep {
                        zeroize_value(v);
                    }
                    keep
                });
            }
            if let Some(init) = connection.get_mut("init_sql").and_then(Value::as_array_mut) {
                init.retain_mut(|v| {
                    let keep = v
                        .as_str()
                        .is_some_and(|s| !crate::properties::sql_carries_secret(s));
                    if !keep {
                        zeroize_value(v);
                    }
                    keep
                });
            }
        }
    }
}

/// `connections.json` 1 → 2: SSH hops gain `host_key_policy`. Version 1 files have no such key,
/// and an absent key means strict, which is exactly the version 1 behavior: nothing to rewrite.
/// Any `host_key_policy` key already present in a version 1 file (hand edit) is removed, so a
/// downgrade can only come from a file written by a NineSQL that knows the field.
fn connections_v1_to_v2(doc: &mut Value) -> Result<(), String> {
    let Some(connections) = doc.get_mut("connections").and_then(Value::as_array_mut) else {
        return Ok(());
    };
    for connection in connections {
        let Some(ssh) = connection.get_mut("ssh").and_then(Value::as_object_mut) else {
            continue;
        };
        ssh.remove("host_key_policy");
        if let Some(jumps) = ssh.get_mut("jump_hosts").and_then(Value::as_array_mut) {
            for jump in jumps {
                if let Some(jump) = jump.as_object_mut() {
                    jump.remove("host_key_policy");
                }
            }
        }
    }
    Ok(())
}

/// `connections.json` 2 → 3: connections gain `password_exchange`. Version 2 files have no such
/// key, and an absent key means `any`, exactly the version 2 behavior: nothing to rewrite. A
/// `password_exchange` key already present in a version 2 file (hand edit) is removed, so the
/// policy only ever comes from a NineSQL that knows the field and validated it.
fn connections_v2_to_v3(doc: &mut Value) -> Result<(), String> {
    let Some(connections) = doc.get_mut("connections").and_then(Value::as_array_mut) else {
        return Ok(());
    };
    for connection in connections {
        if let Some(connection) = connection.as_object_mut() {
            connection.remove("password_exchange");
        }
    }
    Ok(())
}

/// `connections.json` 3 → 4: connections gain `init_sql_needs_confirmation` (P1-E review, for
/// the P2-C import). Version 3 files have no such key, and an absent key means `false`, exactly
/// the version 3 behavior: nothing to rewrite. Unlike the earlier migrations, a key already
/// present (hand edit) is kept: `true` only adds a confirmation, so it can never weaken the
/// connection. The bump makes an older NineSQL refuse a version 4 file instead of dropping the
/// marker and skipping the confirmation.
fn connections_v3_to_v4(doc: &mut Value) -> Result<(), String> {
    let Some(connections) = doc.get_mut("connections").and_then(Value::as_array_mut) else {
        return Ok(());
    };
    for connection in connections {
        if let Some(connection) = connection.as_object_mut()
            && connection
                .get("init_sql_needs_confirmation")
                .is_some_and(|v| !v.is_boolean())
        {
            // Anything but a boolean would fail the load: read as "needs confirmation".
            connection.insert("init_sql_needs_confirmation".to_owned(), Value::Bool(true));
        }
    }
    Ok(())
}

/// `connections.json` 4 → 5: connections gain `default_schema`, `default_catalog`,
/// `auto_commit` and `disabled_reason`, and a DBeaver origin gains `source_workspace` (P2-C). Version 4 files have no such keys, and absent
/// keys mean exactly the version 4 behavior: nothing to rewrite. Keys already present in a
/// version 4 file (hand edit) are removed, as in the 1 → 2 and 2 → 3 migrations, so these
/// settings only ever come from a NineSQL that knows and validated them; `enabled` is left as
/// it is, so a connection disabled by hand stays disabled.
fn connections_v4_to_v5(doc: &mut Value) -> Result<(), String> {
    let Some(connections) = doc.get_mut("connections").and_then(Value::as_array_mut) else {
        return Ok(());
    };
    for connection in connections {
        if let Some(connection) = connection.as_object_mut() {
            for key in [
                "default_schema",
                "default_catalog",
                "auto_commit",
                "disabled_reason",
            ] {
                if let Some(mut v) = connection.remove(key) {
                    zeroize_value(&mut v);
                }
            }
            if let Some(origin) = connection.get_mut("origin").and_then(Value::as_object_mut) {
                origin.remove("source_workspace");
            }
        }
    }
    Ok(())
}

const CONNECTIONS_MIGRATIONS: &[Migration] = &[
    Migration {
        from: 1,
        description: "add the per-hop SSH host key policy (strict when absent)",
        apply: connections_v1_to_v2,
    },
    Migration {
        from: 2,
        description: "add the password exchange policy (any when absent)",
        apply: connections_v2_to_v3,
    },
    Migration {
        from: 3,
        description: "add the imported init SQL marker (not set when absent)",
        apply: connections_v3_to_v4,
    },
    Migration {
        from: 4,
        description: "add the default schema / catalog, the auto-commit override and the disabled reason",
        apply: connections_v4_to_v5,
    },
];

/// `connection-types.json`: built-in types as customized plus user-defined types.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionTypesFile {
    /// Types.
    #[serde(default)]
    pub types: Vec<ConnectionType>,
}

impl Document for ConnectionTypesFile {
    const NAME: &'static str = "connection-types";
    const FORMAT: Format = Format::Json;
    const SCHEMA_VERSION: u32 = 1;
}

/// The connections of a project, with unique ids.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Connections {
    items: Vec<Connection>,
}

impl Connections {
    /// Builds the set, rejecting duplicate ids.
    ///
    /// # Errors
    /// [`ConnectionError::DuplicateId`].
    pub fn from_vec(items: Vec<Connection>) -> Result<Self, ConnectionError> {
        let mut set = Self::default();
        for c in items {
            set.insert(c)?;
        }
        Ok(set)
    }

    /// All connections, in display order.
    pub fn iter(&self) -> impl Iterator<Item = &Connection> {
        self.items.iter()
    }

    /// Number of connections.
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether there is no connection.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// The connection with that id.
    #[must_use]
    pub fn get(&self, id: ConnectionId) -> Option<&Connection> {
        self.items.iter().find(|c| c.id == id)
    }

    /// Mutable access to the connection with that id.
    pub fn get_mut(&mut self, id: ConnectionId) -> Option<&mut Connection> {
        self.items.iter_mut().find(|c| c.id == id)
    }

    /// The connection imported from that DBeaver data source, from that DBeaver project of
    /// that workspace (canonical path), for idempotent re-import (ADR-0006 §8). Matching on the
    /// id alone would let another workspace that reuses the id update this connection
    /// (P2-C review, H1).
    #[must_use]
    pub fn find_dbeaver_origin(
        &self,
        dbeaver_id: &str,
        project: &str,
        source_workspace: &std::path::Path,
    ) -> Option<&Connection> {
        self.items.iter().find(|c| {
            matches!(&c.origin, crate::connection::Origin::Dbeaver {
                dbeaver_id: d,
                project: Some(p),
                source_workspace: Some(w),
            } if d == dbeaver_id && p == project && w == source_workspace)
        })
    }

    /// The first connection with that DBeaver data source id, whatever its project or
    /// workspace. **Not** a re-import match: see [`Self::find_dbeaver_origin`].
    #[must_use]
    pub fn find_by_dbeaver_id(&self, dbeaver_id: &str) -> Option<&Connection> {
        self.items.iter().find(|c| {
            matches!(&c.origin, crate::connection::Origin::Dbeaver { dbeaver_id: d, .. } if d == dbeaver_id)
        })
    }

    /// Appends a connection.
    ///
    /// # Errors
    /// [`ConnectionError::DuplicateId`].
    pub fn insert(&mut self, connection: Connection) -> Result<(), ConnectionError> {
        if self.get(connection.id).is_some() {
            return Err(ConnectionError::DuplicateId(connection.id));
        }
        self.items.push(connection);
        Ok(())
    }

    /// Removes and returns a connection.
    pub fn remove(&mut self, id: ConnectionId) -> Option<Connection> {
        let index = self.items.iter().position(|c| c.id == id)?;
        Some(self.items.remove(index))
    }
}

/// Data removed from one connection while loading (see [`Workspace::load_connections`]).
#[derive(Debug)]
pub struct LoadIssue {
    /// Connection concerned.
    pub connection: ConnectionId,
    /// Its name, for the report.
    pub name: String,
    /// What was removed and why (never the value).
    pub problem: ConnectionError,
}

/// Result of [`Workspace::load_connections`]. Callers must show `issues` to the user.
#[derive(Debug, Default)]
#[must_use = "load issues (removed connection data) must be shown to the user"]
pub struct LoadedConnections {
    /// The connections, sanitized and valid.
    pub connections: Connections,
    /// Data removed per connection, to show to the user.
    pub issues: Vec<LoadIssue>,
    /// Unknown keys that looked like secrets (paths only); dropped on the next save.
    pub secret_like_keys: Vec<String>,
}

/// [`DriverRegistry::validate_connection`] for enabled connections, the model rules for
/// disabled ones (kept for a later version, they may have no driver).
fn check_for_driver(registry: &DriverRegistry, c: &Connection) -> Result<(), WorkspaceError> {
    let result = if c.enabled {
        registry.validate_connection(c)
    } else {
        c.validate().map_err(CoreError::from)
    };
    result.map_err(|source| WorkspaceError::Core {
        name: c.name.clone(),
        source: Box::new(source),
    })
}

/// What a save must keep from the copy of a connection on disk, whatever the caller's copy says
/// (P2-C): the imported init SQL marker (a UI copy that lost it must not clear it: one more
/// confirmation, never one less) and the disabled reason (only
/// [`Workspace::enable_connection`] clears it, after [`Connection::try_enable`]).
///
/// - Marker: set again when the disk copy has it and the new copy holds the same init SQL. New
///   init SQL without the marker is checked at open against the disk anyway
///   ([`Workspace::init_sql_marked_on_disk`]).
/// - Disabled reason: any change of it (dropped, added, other kind or message) is refused
///   ([`ConnectionError::InvalidField`] on `disabled_reason`), unless `allow_reason_change` (the
///   import, which recomputes the reason from the source). Turning a `route` reason into an
///   `invalid` one would otherwise let a connection be enabled without confirming its route.
fn carry_protected_state(
    disk: &Connections,
    next: &mut Connections,
    allow_reason_change: bool,
) -> Result<(), WorkspaceError> {
    for c in &mut next.items {
        let Some(old) = disk.get(c.id) else {
            continue;
        };
        if old.init_sql_needs_confirmation && !c.init_sql.is_empty() && old.init_sql == c.init_sql {
            c.init_sql_needs_confirmation = true;
        }
        if !allow_reason_change && old.disabled_reason != c.disabled_reason {
            return Err(WorkspaceError::InvalidConnection {
                name: c.name.clone(),
                source: ConnectionError::InvalidField {
                    field: "disabled_reason".to_owned(),
                    reason: "is set by NineSQL and only cleared by enabling the connection once its cause is fixed",
                },
            });
        }
    }
    Ok(())
}

/// The process-wide write guard of one `connections.json`, keyed by its canonical path (the
/// path as given when it cannot be resolved), so two [`Workspace::open`] of the same project
/// share it (P1-E review, L3). Entries whose guard is no longer used are dropped.
fn connections_guard_for(file: &std::path::Path) -> Arc<Mutex<()>> {
    static GUARDS: std::sync::OnceLock<
        Mutex<std::collections::HashMap<std::path::PathBuf, std::sync::Weak<Mutex<()>>>>,
    > = std::sync::OnceLock::new();
    // The directory exists (created by `Workspace::open`); the file may not exist yet.
    let key = match (file.parent(), file.file_name()) {
        (Some(dir), Some(name)) => std::fs::canonicalize(dir)
            .map(|d| d.join(name))
            .unwrap_or_else(|_| file.to_path_buf()),
        _ => file.to_path_buf(),
    };
    let mut guards = GUARDS
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    guards.retain(|_, g| g.strong_count() > 0);
    if let Some(guard) = guards.get(&key).and_then(std::sync::Weak::upgrade) {
        return guard;
    }
    let guard = Arc::new(Mutex::new(()));
    guards.insert(key, Arc::downgrade(&guard));
    guard
}

/// What [`Workspace::clear_init_sql_marker`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MarkerClear {
    /// The marker was set on these statements and is now cleared on disk.
    Cleared,
    /// The connection on disk holds these statements without the marker: nothing to do.
    AlreadyClear,
    /// The init SQL on disk is not the confirmed one (changed meanwhile, for example by a
    /// re-import): the marker is left as it is.
    Changed,
    /// No connection with that id on disk.
    NotFound,
}

impl MarkerClear {
    /// Whether, after this, `connections.json` holds the confirmed statements without the
    /// marker.
    #[must_use]
    pub fn is_clear(self) -> bool {
        matches!(self, Self::Cleared | Self::AlreadyClear)
    }
}

/// One project of the NineSQL workspace on disk. Holds the workspace lock while any clone is
/// alive.
#[derive(Debug, Clone)]
pub struct Workspace {
    paths: Paths,
    project: ProjectName,
    project_id: ProjectId,
    project_paths: ProjectPaths,
    /// Serializes the writes of `connections.json` within the process, so a read-modify-write
    /// ([`Self::clear_init_sql_marker`]) cannot lose a save made in between. Shared by every
    /// `Workspace` of the process that opened the same project ([`connections_guard_for`]).
    connections_guard: Arc<Mutex<()>>,
    _lock: Arc<DirLock>,
}

impl Workspace {
    /// Opens a project: creates the NineSQL directories (`0700`, owned by the current user,
    /// never through a symbolic link), takes the workspace lock, then reads `project.json`
    /// (migrating it) or creates it with a new [`ProjectId`].
    ///
    /// Several `Workspace` values of one process share the lock; another process gets
    /// [`ninesql_storage::StorageError::Locked`].
    ///
    /// # Errors
    /// Storage errors, including [`ninesql_storage::StorageError::Locked`],
    /// [`ninesql_storage::StorageError::NotOwned`] and
    /// [`ninesql_storage::StorageError::WrongFileType`] (a symbolic link in place of a NineSQL
    /// directory).
    pub fn open(paths: Paths, project: ProjectName) -> Result<Self, WorkspaceError> {
        let project_paths = paths.project(&project);
        for dir in [
            paths.config_dir().to_path_buf(),
            paths.data_dir().to_path_buf(),
            paths.state_dir().to_path_buf(),
            paths.workspace_dir(),
            project_paths.dir().to_path_buf(),
            project_paths.scripts_dir(),
            project_paths.diagrams_dir(),
        ] {
            ensure_private_dir(&dir)?;
        }
        let lock = DirLock::acquire(&paths.lock_file())?;
        let project_file = project_paths.project_file();
        let project_id = match document::load::<ProjectFile>(&project_file)? {
            Some(loaded) => loaded.document.id,
            None => {
                let file = ProjectFile {
                    id: ProjectId::new_random(),
                    name: project.clone(),
                    description: None,
                };
                document::save(&project_file, &file)?;
                file.id
            }
        };
        let connections_guard = connections_guard_for(&project_paths.connections_file());
        Ok(Self {
            paths,
            project,
            project_id,
            project_paths,
            connections_guard,
            _lock: lock,
        })
    }

    /// Stable identifier of the project, from `project.json`. Secret Service stores of this
    /// project are scoped with it (`SecretServiceStore::new(&workspace.project_id().to_string())`),
    /// never with the mutable project name (N6).
    #[must_use]
    pub fn project_id(&self) -> ProjectId {
        self.project_id
    }

    /// NineSQL paths.
    #[must_use]
    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    /// Project name.
    #[must_use]
    pub fn project(&self) -> &ProjectName {
        &self.project
    }

    /// Paths of the project.
    #[must_use]
    pub fn project_paths(&self) -> &ProjectPaths {
        &self.project_paths
    }

    /// Loads `connections.json` (empty when missing), migrating it if needed.
    ///
    /// Each connection is [sanitized](Connection::sanitize): secret-bearing or reserved driver
    /// properties and init SQL with password literals are removed and reported in
    /// [`LoadedConnections::issues`], so one bad connection does not block the project. When
    /// anything was removed, the file is rewritten at once without it (atomic, `0600`), not at
    /// the next save (N7.2); so are unknown secret-looking keys (by the storage layer). Any other
    /// invalid data fails the load.
    ///
    /// # Errors
    /// Storage errors, invalid or duplicate connections. On a validation error the file keeps its
    /// invalid data for the user to fix, but secret-bearing properties, init SQL and
    /// secret-looking keys are removed from it first (atomic rewrite of the raw file).
    #[must_use = "load issues (removed connection data) must be shown to the user"]
    pub fn load_connections(&self) -> Result<LoadedConnections, WorkspaceError> {
        let _guard = self.guard();
        self.load_connections_unguarded()
    }

    fn guard(&self) -> MutexGuard<'_, ()> {
        self.connections_guard
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn load_connections_unguarded(&self) -> Result<LoadedConnections, WorkspaceError> {
        let path = self.project_paths.connections_file();
        let Some(loaded) = document::load::<ConnectionsFile>(&path)? else {
            return Ok(LoadedConnections::default());
        };
        let mut issues = Vec::new();
        let mut items = loaded.document.connections;
        for c in &mut items {
            for problem in c.sanitize() {
                tracing::warn!(connection = %c.id, problem = %problem, "removed unsafe connection data on load");
                issues.push(LoadIssue {
                    connection: c.id,
                    name: c.name.clone(),
                    problem,
                });
            }
            if let Err(source) = c.validate() {
                let name = c.name.clone();
                self.scrub_after_failed_load(&path);
                return Err(WorkspaceError::InvalidConnection { name, source });
            }
        }
        let connections = match Connections::from_vec(items) {
            Ok(connections) => connections,
            Err(e) => {
                self.scrub_after_failed_load(&path);
                return Err(e.into());
            }
        };
        if !issues.is_empty() {
            self.save_connections_unguarded(&connections)?;
            tracing::warn!(
                project = %self.project,
                removed = issues.len(),
                "rewrote connections.json without the removed connection data"
            );
        }
        tracing::debug!(project = %self.project, count = connections.len(), "loaded connections");
        Ok(LoadedConnections {
            connections,
            issues,
            secret_like_keys: loaded.secret_like_keys,
        })
    }

    /// After a load that parsed but failed validation, removes secret-looking data from the file
    /// as it is on disk ([`document::scrub_file`] with [`ConnectionsFile::scrub`]), so a secret in
    /// another connection does not stay on disk until the user fixes the invalid data (I1). The
    /// invalid data itself is kept for the user to fix. Failures are logged: the validation error
    /// is what the caller reports.
    fn scrub_after_failed_load(&self, path: &std::path::Path) {
        match document::scrub_file::<ConnectionsFile>(path) {
            Ok(true) => tracing::warn!(
                project = %self.project,
                "removed secret-looking data from connections.json, which has invalid data"
            ),
            Ok(false) => {}
            Err(e) => tracing::warn!(
                project = %self.project,
                error = %e,
                "could not remove secret-looking data from connections.json"
            ),
        }
    }

    /// Validates and atomically writes `connections.json`.
    ///
    /// # Errors
    /// Invalid connections (nothing is written), storage errors.
    pub fn save_connections(&self, connections: &Connections) -> Result<(), WorkspaceError> {
        let _guard = self.guard();
        self.save_connections_unguarded(connections)
    }

    fn save_connections_unguarded(&self, connections: &Connections) -> Result<(), WorkspaceError> {
        for c in connections.iter() {
            c.validate()
                .map_err(|source| WorkspaceError::InvalidConnection {
                    name: c.name.clone(),
                    source,
                })?;
        }
        let file = ConnectionsFile {
            connections: connections.items.clone(),
        };
        document::save(&self.project_paths.connections_file(), &file)?;
        tracing::debug!(project = %self.project, count = connections.len(), "saved connections");
        Ok(())
    }

    /// Clears [`Connection::init_sql_needs_confirmation`] of connection `id` in
    /// `connections.json`, if its init SQL on disk is still exactly `statements` (what the user
    /// confirmed). Called by the session manager once the user confirmed imported init SQL.
    /// Load, change and save run under the process-wide guard of `connections.json`, so a save
    /// made meanwhile is not lost (a save of an older copy afterwards may set the marker again:
    /// one more confirmation, never one less).
    ///
    /// Load issues (unsafe data found in the file, P1-E review L4): as with every
    /// [`Self::load_connections`], they are removed from the file at once and logged at `warn`
    /// level; they are not returned here. They can only come from an edit of the file made
    /// outside NineSQL since the UI loaded it, and the UI reports them at its next load.
    ///
    /// # Errors
    /// Storage errors and the load / validation errors of [`Self::load_connections`].
    pub fn clear_init_sql_marker(
        &self,
        id: ConnectionId,
        statements: &[String],
    ) -> Result<MarkerClear, WorkspaceError> {
        let _guard = self.guard();
        let mut connections = self.load_connections_unguarded()?.connections;
        let Some(c) = connections.get_mut(id) else {
            return Ok(MarkerClear::NotFound);
        };
        if c.init_sql != statements {
            return Ok(MarkerClear::Changed);
        }
        if !c.init_sql_needs_confirmation {
            return Ok(MarkerClear::AlreadyClear);
        }
        c.init_sql_needs_confirmation = false;
        self.save_connections_unguarded(&connections)?;
        tracing::info!(project = %self.project, connection = %id, "imported init SQL confirmed");
        Ok(MarkerClear::Cleared)
    }

    /// Whether the init SQL of `connection` must be confirmed as imported, from what
    /// `connections.json` holds (P1-E review, M2: the session manager does not trust the
    /// request's marker alone). `true` when the connection on disk carries the marker, or holds
    /// other init SQL than `connection` (an unsaved change of the init SQL counts as not
    /// confirmed). `false` when it is not on disk (a connection tested before its first save:
    /// the request's marker applies).
    ///
    /// # Errors
    /// The load errors of [`Self::load_connections`] (the caller treats them as marked).
    pub fn init_sql_marked_on_disk(&self, connection: &Connection) -> Result<bool, WorkspaceError> {
        let loaded = self.load_connections()?;
        Ok(loaded
            .connections
            .get(connection.id)
            .is_some_and(|c| c.init_sql_needs_confirmation || c.init_sql != connection.init_sql))
    }

    /// [`Self::save_connections`] after [`DriverRegistry::validate_connection`] of every
    /// connection: what the P1-C dialog calls, so a connection its driver would refuse at
    /// connect (property, authentication, password exchange, TLS mode…) is never saved.
    /// Disabled connections (engines without a driver, imported for later, ADR-0006 §5) are
    /// checked against the model rules only.
    ///
    /// Since P2-C it also keeps what the disk copy protects ([`carry_protected_state`]): the
    /// imported init SQL marker of a connection whose init SQL is unchanged is kept even if
    /// `connections` lost it, and a disabled reason cannot be dropped (use
    /// [`Self::enable_connection`]). The file is read under the write guard first.
    ///
    /// # Errors
    /// [`WorkspaceError::Core`] with the first connection its driver refuses (nothing is
    /// written), [`WorkspaceError::InvalidConnection`] for a dropped disabled reason, then the
    /// load and save errors of [`Self::load_connections`] and [`Self::save_connections`].
    pub fn save_connections_checked(
        &self,
        connections: &Connections,
        registry: &DriverRegistry,
    ) -> Result<(), WorkspaceError> {
        for c in connections.iter() {
            check_for_driver(registry, c)?;
        }
        let _guard = self.guard();
        let disk = self.load_connections_unguarded()?.connections;
        let mut next = connections.clone();
        carry_protected_state(&disk, &mut next, false)?;
        self.save_connections_unguarded(&next)
    }

    /// Replaces one connection (same id) and saves `connections.json`, so the UI does not
    /// rewrite the whole list to edit one connection (wave 3 follow-up). The connection is
    /// checked like [`Self::save_connections_checked`] (driver rules for an enabled connection,
    /// model rules for a disabled one), and the file is read, changed and written under the
    /// process-wide write guard, so a marker clear or another save made meanwhile is not lost.
    /// The protected state of the disk copy is kept ([`carry_protected_state`]): the imported
    /// init SQL marker of unchanged init SQL, and the disabled reason (refused if dropped).
    ///
    /// `connections` (the caller's copy) is updated with what was written, the other entries
    /// refreshed from the file.
    ///
    /// # Errors
    /// [`ConnectionError::NotFound`] when no connection with that id is on disk; the check
    /// errors; storage errors. Nothing is written on error and `connections` is unchanged.
    pub fn update_connection(
        &self,
        connections: &mut Connections,
        connection: Connection,
        registry: &DriverRegistry,
    ) -> Result<(), WorkspaceError> {
        check_for_driver(registry, &connection)?;
        let _guard = self.guard();
        let disk = self.load_connections_unguarded()?.connections;
        let mut next = disk.clone();
        let slot = next
            .get_mut(connection.id)
            .ok_or(ConnectionError::NotFound(connection.id))?;
        *slot = connection;
        carry_protected_state(&disk, &mut next, false)?;
        self.save_connections_unguarded(&next)?;
        *connections = next;
        Ok(())
    }

    /// Enables connection `id` through [`Connection::try_enable`] (the cause of its
    /// [`DisabledReason`](crate::DisabledReason) must be gone; `route_fixed` confirms a route
    /// the import could not reproduce was set up again) and saves it, under the write guard.
    ///
    /// # Errors
    /// [`ConnectionError::NotFound`]; [`WorkspaceError::Core`] when the driver refuses it or the
    /// route is not confirmed (the message says which); storage errors.
    pub fn enable_connection(
        &self,
        connections: &mut Connections,
        id: ConnectionId,
        route_fixed: bool,
        registry: &DriverRegistry,
    ) -> Result<(), WorkspaceError> {
        let _guard = self.guard();
        let mut next = self.load_connections_unguarded()?.connections;
        let c = next.get_mut(id).ok_or(ConnectionError::NotFound(id))?;
        let name = c.name.clone();
        c.try_enable(registry, route_fixed)
            .map_err(|e| WorkspaceError::Core {
                name,
                source: Box::new(match e {
                    crate::registry::EnableError::Refused(core) => core,
                    route @ crate::registry::EnableError::RouteNotConfirmed(_) => {
                        CoreError::Runtime(route.to_string())
                    }
                }),
            })?;
        self.save_connections_unguarded(&next)?;
        *connections = next;
        tracing::info!(project = %self.project, connection = %id, "connection enabled");
        Ok(())
    }

    /// The bulk write of the DBeaver import (P2-C): every connection is checked
    /// ([`check_for_driver`]), the file is read and written under the write guard, connections
    /// of `upserts` replace those with the same id or are appended, and the imported init SQL
    /// marker of the disk copy is kept like [`Self::save_connections_checked`] does. Unlike the
    /// UI paths, the import may set or clear a disabled reason (it recomputes it from the
    /// source). Returns the connections as written, or `None` without writing anything when
    /// the disk copy of an upserted connection is not `expected` (the saved copy the import
    /// planned against; `None` for a new connection): a UI edit made since the preview is never
    /// overwritten (P2-C review, L2).
    pub(crate) fn save_imported(
        &self,
        upserts: Vec<(Connection, Option<Connection>)>,
        registry: &DriverRegistry,
    ) -> Result<Option<Connections>, WorkspaceError> {
        for (c, _) in &upserts {
            check_for_driver(registry, c)?;
        }
        let _guard = self.guard();
        let disk = self.load_connections_unguarded()?.connections;
        if upserts
            .iter()
            .any(|(c, expected)| disk.get(c.id) != expected.as_ref())
        {
            return Ok(None);
        }
        let mut next = disk.clone();
        for (c, _) in upserts {
            match next.get_mut(c.id) {
                Some(slot) => *slot = c,
                None => next.insert(c)?,
            }
        }
        carry_protected_state(&disk, &mut next, true)?;
        self.save_connections_unguarded(&next)?;
        Ok(Some(next))
    }

    /// [`Self::create_connection`] after [`DriverRegistry::validate_connection`]: nothing is
    /// stored (no secret, no file) when the driver would refuse the connection.
    ///
    /// # Errors
    /// [`WorkspaceError::Core`] when the driver refuses it, then the errors of
    /// [`Self::create_connection`].
    pub fn create_connection_checked(
        &self,
        connections: &mut Connections,
        connection: Connection,
        secrets: Vec<(SecretSlot, SecretString)>,
        store: StoreKind,
        stores: &SecretStores,
        registry: &DriverRegistry,
    ) -> Result<ConnectionId, WorkspaceError> {
        check_for_driver(registry, &connection)?;
        self.create_connection(connections, connection, secrets, store, stores)
    }

    /// Loads `connection-types.json`; built-in types are always present.
    ///
    /// # Errors
    /// Storage errors.
    pub fn load_connection_types(&self) -> Result<ConnectionTypes, WorkspaceError> {
        let path = self.project_paths.connection_types_file();
        Ok(match document::load::<ConnectionTypesFile>(&path)? {
            Some(loaded) => ConnectionTypes::from_saved(loaded.document.types),
            None => ConnectionTypes::default(),
        })
    }

    /// Atomically writes `connection-types.json`.
    ///
    /// # Errors
    /// Storage errors.
    pub fn save_connection_types(&self, types: &ConnectionTypes) -> Result<(), WorkspaceError> {
        let file = ConnectionTypesFile {
            types: types.all().to_vec(),
        };
        Ok(document::save(
            &self.project_paths.connection_types_file(),
            &file,
        )?)
    }

    /// Loads the navigator filters (`navigator.json`, F-NAV-04); a missing file means no
    /// filter. Entries with an unknown kind or an invalid pattern are dropped and returned.
    ///
    /// # Errors
    /// Storage errors (a newer `schema_version` included; the file is left untouched).
    pub fn load_navigator_filters(
        &self,
    ) -> Result<
        (
            crate::metadata::NavigatorFilters,
            Vec<crate::metadata::FilterIssue>,
        ),
        WorkspaceError,
    > {
        let path = self.project_paths.navigator_file();
        Ok(
            match document::load::<crate::metadata::NavigatorFile>(&path)? {
                Some(loaded) => crate::metadata::NavigatorFilters::from_file(loaded.document),
                None => Default::default(),
            },
        )
    }

    /// Atomically writes `navigator.json`.
    ///
    /// # Errors
    /// Storage errors.
    pub fn save_navigator_filters(
        &self,
        filters: &crate::metadata::NavigatorFilters,
    ) -> Result<(), WorkspaceError> {
        Ok(document::save(
            &self.project_paths.navigator_file(),
            &filters.to_file(),
        )?)
    }

    /// Creates a connection: each secret goes to `store` and the matching credential field of
    /// `connection` is pointed at it; then `connections.json` is saved.
    ///
    /// Credential fields not listed in `secrets` keep their value (usually
    /// [`crate::SecretSource::Prompt`]). Secrets are zeroized when this returns. If saving the
    /// file fails, the secrets just stored are deleted again (best effort).
    ///
    /// # Errors
    /// Invalid connection, duplicate id, a slot without credential field, secret store or
    /// storage errors. `connections` is unchanged on error.
    pub fn create_connection(
        &self,
        connections: &mut Connections,
        mut connection: Connection,
        secrets: Vec<(SecretSlot, SecretString)>,
        store: StoreKind,
        stores: &SecretStores,
    ) -> Result<ConnectionId, WorkspaceError> {
        connection.validate()?;
        if connections.get(connection.id).is_some() {
            return Err(ConnectionError::DuplicateId(connection.id).into());
        }
        // Point fields at their references first, so a bad slot fails before anything is stored.
        let mut pending = Vec::with_capacity(secrets.len());
        for (slot, secret) in secrets {
            let id =
                SecretId::new(format!("{}:{}", connection.id, slot.id_suffix())).map_err(|_| {
                    ConnectionError::InvalidField {
                        field: "id".to_owned(),
                        reason: "cannot be used to name a secret",
                    }
                })?;
            let reference = SecretRef::new(store, id);
            let kind = connection.set_secret_ref(slot, reference.clone())?;
            pending.push((reference, kind, secret));
        }

        let mut stored: Vec<SecretRef> = Vec::with_capacity(pending.len());
        let mut next = connections.clone();
        let result = (|| {
            for (reference, kind, secret) in &pending {
                let meta = SecretMeta {
                    connection_id: connection.id.to_string(),
                    kind: *kind,
                    label: format!("NineSQL: {}", connection.name),
                };
                stores.save(reference, &meta, secret)?;
                stored.push(reference.clone());
            }
            let id = connection.id;
            next.insert(connection)?;
            self.save_connections(&next)?;
            Ok::<_, WorkspaceError>(id)
        })();
        drop(pending);
        match result {
            Ok(id) => {
                *connections = next;
                tracing::info!(project = %self.project, connection = %id, "connection created");
                Ok(id)
            }
            Err(e) => {
                for reference in &stored {
                    if let Err(del) = stores.delete(reference) {
                        tracing::warn!(reference = %reference, error = %del, "could not roll back a stored secret");
                    }
                }
                Err(e)
            }
        }
    }

    /// Removes a connection, saves `connections.json`, then deletes its saved secrets. A secret
    /// that cannot be deleted is logged and left in the store.
    ///
    /// # Errors
    /// [`ConnectionError::NotFound`], storage errors (then nothing is removed).
    pub fn remove_connection(
        &self,
        connections: &mut Connections,
        id: ConnectionId,
        stores: &SecretStores,
    ) -> Result<Connection, WorkspaceError> {
        let mut next = connections.clone();
        let removed = next.remove(id).ok_or(ConnectionError::NotFound(id))?;
        self.save_connections(&next)?;
        *connections = next;
        for (_, _, reference) in removed.secret_refs() {
            if let Err(e) = stores.delete(reference) {
                tracing::warn!(reference = %reference, error = %e, "could not delete a secret of a removed connection");
            }
        }
        tracing::info!(project = %self.project, connection = %id, "connection removed");
        Ok(removed)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).
mod tests {
    use super::*;

    /// P2-C review, L2: the import's bulk write refuses (writes nothing) when a connection to
    /// update is not on disk as planned.
    #[test]
    fn save_imported_refuses_a_connection_changed_since_the_plan() {
        use crate::connection::{Auth, Endpoint, EngineId};
        use crate::connection_type::ConnectionTypeId;
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(&ninesql_storage::XdgDirs {
            config_home: tmp.path().join("config"),
            data_home: tmp.path().join("data"),
            state_home: tmp.path().join("state"),
        });
        let ws = Workspace::open(paths, ProjectName::default_project()).unwrap();
        let registry = DriverRegistry::new();
        let mut c = Connection::new(
            "c",
            EngineId::new("nodriver").unwrap(),
            Endpoint::File {
                path: "/tmp/x.db".into(),
            },
            Auth::None,
            ConnectionTypeId::dev(),
        );
        c.enabled = false;
        let written = ws
            .save_imported(vec![(c.clone(), None)], &registry)
            .unwrap()
            .unwrap();
        assert_eq!(written.len(), 1);
        let planned = c.clone();
        let mut edited = c.clone();
        edited.name = "edited".into();
        ws.save_connections(&Connections::from_vec(vec![edited]).unwrap())
            .unwrap();
        let mut update = c;
        update.description = Some("import".into());
        assert!(
            ws.save_imported(vec![(update, Some(planned))], &registry)
                .unwrap()
                .is_none()
        );
        let on_disk = ws.load_connections().unwrap().connections;
        assert_eq!(on_disk.iter().next().unwrap().name, "edited");
    }

    /// P1-E review, L3: two `Workspace` values of one project share the write guard of
    /// `connections.json`; another project has its own. (Reaching the same project through
    /// another path in one process is refused by the workspace lock already.)
    #[test]
    fn workspaces_of_one_project_share_the_connections_guard() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = |root: &std::path::Path| {
            Paths::new(&ninesql_storage::XdgDirs {
                config_home: root.join("config"),
                data_home: root.join("data"),
                state_home: root.join("state"),
            })
        };
        let a = Workspace::open(paths(tmp.path()), ProjectName::default_project()).unwrap();
        let b = Workspace::open(paths(tmp.path()), ProjectName::default_project()).unwrap();
        assert!(Arc::ptr_eq(&a.connections_guard, &b.connections_guard));
        let other = ProjectName::new("other").unwrap();
        let c = Workspace::open(paths(tmp.path()), other).unwrap();
        assert!(!Arc::ptr_eq(&a.connections_guard, &c.connections_guard));
    }
}
