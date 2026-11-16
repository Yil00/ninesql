//! The editor's object completion: a `MetadataProvider` over the core's
//! `MetadataService::completion_snapshot`. It reads what the metadata cache holds and never
//! loads (I6); [`resolve_default_schema`] finds the schema unqualified names resolve in and warms
//! it (`warm_schema`) when an editor binds to a connection.

use std::cell::RefCell;
use std::rc::Rc;

use ninesql_core::driver_api::ObjectKind as CoreKind;
use ninesql_core::driver_api::metadata::ObjectPath;
use ninesql_core::{CompletionSnapshot, ConnectionId, MetadataService, NodeKey};
use ninesql_editor::completion::{ColumnEntry, MetadataProvider, ObjectEntry, ObjectKind};

/// Completion data of one connection, read from the metadata cache at each request.
pub struct MetadataCompletion {
    service: MetadataService,
    connection: ConnectionId,
    /// The default schema, once known (set by [`resolve_default_schema`]).
    default_schema: Rc<RefCell<Option<String>>>,
}

impl MetadataCompletion {
    /// A provider for `connection`; `default_schema` is shared with the code that resolves it.
    #[must_use]
    pub fn new(
        service: MetadataService,
        connection: ConnectionId,
        default_schema: Rc<RefCell<Option<String>>>,
    ) -> Self {
        Self {
            service,
            connection,
            default_schema,
        }
    }

    fn snapshot(&self) -> Option<std::sync::Arc<CompletionSnapshot>> {
        self.service.completion_snapshot(self.connection)
    }
}

fn editor_kind(kind: CoreKind) -> Option<ObjectKind> {
    match kind {
        CoreKind::Table | CoreKind::Collection => Some(ObjectKind::Table),
        CoreKind::View | CoreKind::MaterializedView => Some(ObjectKind::View),
        CoreKind::Routine => Some(ObjectKind::Function),
        _ => None,
    }
}

impl MetadataProvider for MetadataCompletion {
    fn schemas(&self) -> Vec<String> {
        self.snapshot()
            .map(|s| s.schemas().to_vec())
            .unwrap_or_default()
    }

    fn default_schema(&self) -> Option<String> {
        self.default_schema.borrow().clone()
    }

    fn objects(&self, schema: Option<&str>) -> Vec<ObjectEntry> {
        let Some(snapshot) = self.snapshot() else {
            return Vec::new();
        };
        let default = self.default_schema();
        let schema = schema.or(default.as_deref());
        snapshot
            .objects(schema)
            .iter()
            .filter_map(|o| {
                Some(ObjectEntry {
                    name: o.name.clone(),
                    kind: editor_kind(o.kind)?,
                })
            })
            .collect()
    }

    fn columns(&self, schema: Option<&str>, table: &str) -> Option<Vec<ColumnEntry>> {
        let snapshot = self.snapshot()?;
        let default = self.default_schema();
        let columns = match schema {
            Some(s) => snapshot.columns(Some(s), table),
            None => default
                .as_deref()
                .and_then(|d| snapshot.columns(Some(d), table))
                .or_else(|| snapshot.columns(None, table)),
        }?;
        Some(
            columns
                .iter()
                .map(|c| ColumnEntry {
                    name: c.name.clone(),
                    data_type: c.data_type.clone(),
                })
                .collect(),
        )
    }
}

/// Finds the default schema of a connection and returns its path: the schema (else catalog, on
/// engines without schemas) named `preferred` (the connection's `default_schema`) when set and
/// listed, otherwise the first one the driver reports; `None` when there is none. Only awaits
/// metadata requests (I6).
pub async fn resolve_default_schema(
    service: &MetadataService,
    connection: ConnectionId,
    preferred: Option<&str>,
) -> Option<ObjectPath> {
    let mut parent = ObjectPath::root();
    // Catalog → schema, at most two levels.
    for _ in 0..2 {
        let kinds = service.child_kinds(connection, &parent).ok()?;
        let kind = if kinds.contains(&CoreKind::Schema) {
            CoreKind::Schema
        } else if kinds.contains(&CoreKind::Catalog) {
            CoreKind::Catalog
        } else {
            return (!parent.0.is_empty()).then_some(parent);
        };
        let node = service
            .children(connection, NodeKey::new(parent.clone(), kind))
            .await
            .ok()?;
        let chosen = preferred
            .and_then(|p| node.items.iter().find(|i| i.name() == p))
            .or_else(|| node.items.first())?;
        if kind == CoreKind::Schema {
            return Some(chosen.path.clone());
        }
        parent = chosen.path.clone();
    }
    (!parent.0.is_empty()).then_some(parent)
}
