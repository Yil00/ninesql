//! Workspace files of a project and the connection flows that touch secrets (ADR-0008, I1).
//!
//! On-disk documents:
//! - `project.json`: [`ProjectFile`] (`schema_version` 2)
//! - `connections.json`: [`ConnectionsFile`] (`schema_version` 1)
//! - `connection-types.json`: [`ConnectionTypesFile`] (`schema_version` 1)
//!
//! Secrets given to [`Workspace::create_connection`] go to the secret store; only their
//! [`SecretRef`] reaches `connections.json`.
//!
//! An open [`Workspace`] holds the advisory workspace lock ([`ninesql_storage::lock`]): a second
//! NineSQL process gets [`ninesql_storage::StorageError::Locked`] instead of writing the same
//! files at the same time.

use std::fmt;
use std::sync::Arc;

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
use crate::error::{ConnectionError, WorkspaceError};

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
    /// Version 1 is the first published schema; its fixture is
    /// `tests/fixtures/connections/v1.json`. A bump adds a [`Migration`] here.
    const SCHEMA_VERSION: u32 = 1;
    fn migrations() -> &'static [Migration] {
        &[]
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

    /// The connection imported from that DBeaver data source, for idempotent re-import
    /// (ADR-0006 §8).
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

/// One project of the NineSQL workspace on disk. Holds the workspace lock while any clone is
/// alive.
#[derive(Debug, Clone)]
pub struct Workspace {
    paths: Paths,
    project: ProjectName,
    project_id: ProjectId,
    project_paths: ProjectPaths,
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
        Ok(Self {
            paths,
            project,
            project_id,
            project_paths,
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
            self.save_connections(&connections)?;
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
