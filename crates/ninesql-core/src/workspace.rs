//! Workspace files of a project and the connection flows that touch secrets (ADR-0008, I1).
//!
//! On-disk documents (all `schema_version` 1):
//! - `project.json`: [`ProjectFile`]
//! - `connections.json`: [`ConnectionsFile`]
//! - `connection-types.json`: [`ConnectionTypesFile`]
//!
//! Secrets given to [`Workspace::create_connection`] go to the secret store; only their
//! [`SecretRef`] reaches `connections.json`.

use ninesql_secrets::{SecretId, SecretMeta, SecretRef, SecretStores, SecretString, StoreKind};
use ninesql_storage::document::{self, Document, Format, Migration};
use ninesql_storage::fs::ensure_private_dir;
use ninesql_storage::{Paths, ProjectName, ProjectPaths};
use serde::{Deserialize, Serialize};

use crate::connection::{Connection, ConnectionId, SecretSlot};
use crate::connection_type::{ConnectionType, ConnectionTypes};
use crate::error::{ConnectionError, WorkspaceError};

/// `project.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectFile {
    /// Project name (same as its directory name).
    pub name: ProjectName,
    /// Free text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl Document for ProjectFile {
    const NAME: &'static str = "project";
    const FORMAT: Format = Format::Json;
    const SCHEMA_VERSION: u32 = 1;
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

/// One project of the NineSQL workspace on disk.
#[derive(Debug, Clone)]
pub struct Workspace {
    paths: Paths,
    project: ProjectName,
    project_paths: ProjectPaths,
}

impl Workspace {
    /// Opens a project, creating the NineSQL directories (`0700`) and `project.json` if missing.
    ///
    /// # Errors
    /// Storage errors.
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
        let project_file = project_paths.project_file();
        if document::load::<ProjectFile>(&project_file)?.is_none() {
            document::save(
                &project_file,
                &ProjectFile {
                    name: project.clone(),
                    description: None,
                },
            )?;
        }
        Ok(Self {
            paths,
            project,
            project_paths,
        })
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
    /// [`LoadedConnections::issues`], so one bad connection does not block the project. The next
    /// save drops them from disk. Any other invalid data fails the load.
    ///
    /// # Errors
    /// Storage errors, invalid or duplicate connections. The file is left untouched on error.
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
            c.validate()
                .map_err(|source| WorkspaceError::InvalidConnection {
                    name: c.name.clone(),
                    source,
                })?;
        }
        let connections = Connections::from_vec(items)?;
        tracing::debug!(project = %self.project, count = connections.len(), "loaded connections");
        Ok(LoadedConnections {
            connections,
            issues,
            secret_like_keys: loaded.secret_like_keys,
        })
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
