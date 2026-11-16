//! Completion data from the metadata cache: what the editor's `MetadataProvider` reads. Built
//! from cached nodes only; never loads. Object kinds and column details are read through
//! [`super::model`].

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use ninesql_driver_api::ObjectKind;

use super::NodeChildren;
use super::model;
use crate::connection::ConnectionId;

/// Most objects (of every kind together) a snapshot holds.
pub const MAX_SNAPSHOT_ITEMS: usize = 250_000;

/// One object of a schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionObject {
    /// Unquoted name.
    pub name: String,
    /// Kind (table, view, materialized view, routine, sequence…).
    pub kind: ObjectKind,
}

/// One column of a table or view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionColumn {
    /// Unquoted name.
    pub name: String,
    /// Engine type name, when known.
    pub data_type: Option<String>,
    /// 1-based position (`0` when unknown).
    pub ordinal: u32,
}

/// A foreign key of a table, as listed. The referenced table and columns are not in the driver
/// API yet (P4-A); `target` stays `None` until they are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignKeyRef {
    /// Schema of the table that owns the key.
    pub schema: Option<String>,
    /// Table that owns the key.
    pub table: String,
    /// Constraint name.
    pub name: String,
    /// Referenced `(schema, table)`, when the driver provides it.
    pub target: Option<(Option<String>, String)>,
}

type TableKey = (Option<String>, String);

/// A read-only view of a connection's cached metadata for completion. Cheap to share (`Arc`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionSnapshot {
    /// The connection.
    pub connection: ConnectionId,
    /// Cache generation it was built from.
    pub generation: u64,
    /// The cache held more objects than [`MAX_SNAPSHOT_ITEMS`]; some are missing.
    pub truncated: bool,
    schemas: Vec<String>,
    objects: BTreeMap<Option<String>, Vec<CompletionObject>>,
    columns: BTreeMap<TableKey, Vec<CompletionColumn>>,
    foreign_keys: BTreeMap<TableKey, Vec<ForeignKeyRef>>,
}

impl CompletionSnapshot {
    pub(crate) fn build(
        connection: ConnectionId,
        generation: u64,
        nodes: &[Arc<NodeChildren>],
    ) -> Self {
        let mut out = Self {
            connection,
            generation,
            truncated: false,
            schemas: Vec::new(),
            objects: BTreeMap::new(),
            columns: BTreeMap::new(),
            foreign_keys: BTreeMap::new(),
        };
        let mut schemas = BTreeSet::new();
        let mut catalogs = BTreeSet::new();
        let mut budget = MAX_SNAPSHOT_ITEMS;
        for node in nodes {
            if budget == 0 {
                out.truncated = true;
                break;
            }
            let take = node.items.len().min(budget);
            if take < node.items.len() {
                out.truncated = true;
            }
            budget -= take;
            let items = &node.items[..take];
            let container = model::container_name(&node.key.parent).map(str::to_owned);
            match node.key.kind {
                ObjectKind::Schema => {
                    schemas.extend(items.iter().map(|i| i.name().to_owned()));
                }
                ObjectKind::Catalog => {
                    catalogs.extend(items.iter().map(|i| i.name().to_owned()));
                }
                ObjectKind::Column => {
                    let Some(table) = node.key.parent.leaf() else {
                        continue;
                    };
                    let mut cols: Vec<CompletionColumn> = items
                        .iter()
                        .map(|i| CompletionColumn {
                            name: i.name().to_owned(),
                            data_type: model::column_type(i).map(str::to_owned),
                            ordinal: model::column_ordinal(i),
                        })
                        .collect();
                    cols.sort_by_key(|c| c.ordinal);
                    out.columns
                        .entry((container, table.name.clone()))
                        .or_default()
                        .extend(cols);
                }
                ObjectKind::ForeignKey => {
                    let Some(table) = node.key.parent.leaf() else {
                        continue;
                    };
                    let fks = items.iter().map(|i| ForeignKeyRef {
                        schema: container.clone(),
                        table: table.name.clone(),
                        name: i.name().to_owned(),
                        target: None,
                    });
                    out.foreign_keys
                        .entry((container.clone(), table.name.clone()))
                        .or_default()
                        .extend(fks);
                }
                kind if model::is_relation(kind)
                    || matches!(
                        kind,
                        ObjectKind::Routine | ObjectKind::Sequence | ObjectKind::Collection
                    ) =>
                {
                    out.objects
                        .entry(container)
                        .or_default()
                        .extend(items.iter().map(|i| CompletionObject {
                            name: i.name().to_owned(),
                            kind,
                        }));
                }
                _ => {}
            }
        }
        // Engines without schemas (catalogs only) complete catalog names as schemas.
        out.schemas = if schemas.is_empty() {
            catalogs.into_iter().collect()
        } else {
            schemas.into_iter().collect()
        };
        for list in out.objects.values_mut() {
            list.sort_by(|a, b| a.name.cmp(&b.name).then(a.kind.cmp(&b.kind)));
            list.dedup();
        }
        out
    }

    /// Schema names (catalog names on engines without schemas), sorted.
    #[must_use]
    pub fn schemas(&self) -> &[String] {
        &self.schemas
    }

    /// Objects of `schema` (`None`: objects whose parent has no schema or catalog), sorted by
    /// name.
    #[must_use]
    pub fn objects(&self, schema: Option<&str>) -> &[CompletionObject] {
        self.objects
            .get(&schema.map(str::to_owned))
            .map_or(&[], Vec::as_slice)
    }

    /// Every schema with cached objects.
    pub fn object_schemas(&self) -> impl Iterator<Item = Option<&str>> {
        self.objects.keys().map(Option::as_deref)
    }

    /// Columns of `schema.table`, in table order, when cached. With `schema = None`, the first
    /// cached table of that name in schema order.
    #[must_use]
    pub fn columns(&self, schema: Option<&str>, table: &str) -> Option<&[CompletionColumn]> {
        match schema {
            Some(s) => self
                .columns
                .get(&(Some(s.to_owned()), table.to_owned()))
                .map(Vec::as_slice),
            None => self
                .columns
                .iter()
                .find(|((_, t), _)| t == table)
                .map(|(_, v)| v.as_slice()),
        }
    }

    /// Foreign keys of `schema.table`, when cached.
    #[must_use]
    pub fn foreign_keys(&self, schema: Option<&str>, table: &str) -> &[ForeignKeyRef] {
        self.foreign_keys
            .get(&(schema.map(str::to_owned), table.to_owned()))
            .map_or(&[], Vec::as_slice)
    }
}
