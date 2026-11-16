//! The navigator's tree model (F-NAV-03), without GPUI: folders and connections, then under an
//! open connection the kinds its driver declares (I8) as groups ("Tables"), the objects of each
//! loaded group, and so on down.
//!
//! - A **group** is one `NodeKey` of the metadata service (the objects of one kind under one
//!   parent). Expanding a group asks the view to load it ([`Effect::Load`]); collapsing it asks
//!   to drop the request ([`Effect::Cancel`]), which cancels the driver call.
//! - Loaded objects stay in the core's `Arc<NodeChildren>`; rows only point into it (a group,
//!   an index), so a group of thousands of objects costs one small row each, and the view
//!   renders only the visible rows (`uniform_list`, I6).
//! - Child kinds come from [`TreeEnv::child_kinds`] (the service's `child_kinds`, declared kinds
//!   only) and are cached per (connection, parent kind) until the connection is forgotten.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use ninesql_core::driver_api::ObjectKind;
use ninesql_core::driver_api::metadata::{ObjectInfo, ObjectPath};
use ninesql_core::{ConnectionId, InvalidationScope, NodeChildren, NodeKey};

/// A group of the tree: one node of the metadata service.
#[derive(Debug, PartialEq, Eq, Hash)]
pub struct GroupRef {
    /// The connection.
    pub connection: ConnectionId,
    /// The node.
    pub key: NodeKey,
}

/// A shared group reference (rows clone it cheaply).
pub type Group = Rc<GroupRef>;

/// What the tree needs from the application.
pub trait TreeEnv {
    /// Whether the connection has an open session.
    fn is_open(&self, connection: ConnectionId) -> bool;
    /// Kinds listed under `parent` that the session declares (empty when not open).
    fn child_kinds(&mut self, connection: ConnectionId, parent: &ObjectPath) -> Vec<ObjectKind>;
}

/// The load state of a group.
#[derive(Debug, Clone)]
pub struct GroupState {
    group: Group,
    /// Shown expanded.
    pub expanded: bool,
    /// Loaded children, if any.
    pub children: Option<Arc<NodeChildren>>,
    /// A load (first or "more") is going on.
    pub loading: bool,
    /// The last load failed (presentable text, no secret: `MetadataError`'s display).
    pub error: Option<Rc<str>>,
}

impl GroupState {
    /// The group.
    #[must_use]
    pub fn group(&self) -> &Group {
        &self.group
    }
}

/// What the view must do after a change of the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Load a group (`MetadataService::children`).
    Load(Group),
    /// Load the next pages of a group (`MetadataService::load_more`).
    LoadMore(Group),
    /// Drop the request of a group (collapse cancels).
    Cancel(Group),
    /// Open a session on a connection (expanding a closed connection).
    Connect(ConnectionId),
}

/// One row of the flattened tree.
#[derive(Debug, Clone)]
pub enum RowKind {
    /// A folder of connections.
    Folder {
        /// `team/eu`.
        path: String,
        /// `eu`.
        name: String,
        /// Shown expanded.
        expanded: bool,
    },
    /// A saved connection.
    Connection {
        /// The connection.
        id: ConnectionId,
        /// Shown expanded.
        expanded: bool,
    },
    /// Under an expanded connection that has no open session.
    NotConnected(ConnectionId),
    /// Under an open connection whose driver declares no object kind (I8).
    NoKinds(ConnectionId),
    /// A group of objects of one kind.
    Group {
        /// The group.
        group: Group,
        /// Shown expanded.
        expanded: bool,
    },
    /// One object of a loaded group.
    Object {
        /// Its group.
        group: Group,
        /// Index in the group's `items`.
        index: usize,
        /// Shown expanded.
        expanded: bool,
        /// Has child kinds.
        expandable: bool,
    },
    /// The group is loading.
    Loading(Group),
    /// More objects of the group are loading.
    LoadingMore(Group),
    /// The group failed to load.
    Error {
        /// The group.
        group: Group,
        /// Presentable message.
        message: Rc<str>,
    },
    /// The server has more objects: "Load more".
    LoadMore(Group),
    /// Objects the navigator filters hid.
    Hidden {
        /// The group.
        group: Group,
        /// How many.
        count: usize,
    },
    /// The group is loaded and empty.
    Empty(Group),
}

/// One row with its depth.
#[derive(Debug, Clone)]
pub struct Row {
    /// Indentation level.
    pub depth: usize,
    /// What it shows.
    pub kind: RowKind,
}

/// A stable identity of a row, to keep the selection across rebuilds.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RowKey {
    /// A folder.
    Folder(String),
    /// A connection.
    Connection(ConnectionId),
    /// The "not connected" / "no kinds" row of a connection.
    ConnectionNote(ConnectionId),
    /// A group.
    Group(ConnectionId, NodeKey),
    /// An object.
    Object(ConnectionId, ObjectPath),
    /// A status row of a group (loading, error, load more, hidden, empty).
    Status(ConnectionId, NodeKey, u8),
}

/// The progress of revealing an object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RevealStep {
    /// Groups on the way are loading; call again when they are loaded.
    Pending(Vec<Effect>),
    /// The object's row.
    Found(RowKey),
    /// The object is not in its loaded group (filtered, not loaded yet, or gone).
    Missing,
}

#[derive(Debug, Clone)]
enum Top {
    Folder {
        path: String,
        name: String,
        depth: usize,
    },
    Connection {
        id: ConnectionId,
        depth: usize,
    },
}

#[derive(Default)]
struct FolderNode {
    folders: BTreeMap<String, FolderNode>,
    connections: Vec<(String, ConnectionId)>,
}

impl FolderNode {
    fn flatten(self, path: &str, depth: usize, out: &mut Vec<Top>) {
        for (name, node) in self.folders {
            let child = if path.is_empty() {
                name.clone()
            } else {
                format!("{path}/{name}")
            };
            out.push(Top::Folder {
                path: child.clone(),
                name,
                depth,
            });
            node.flatten(&child, depth + 1, out);
        }
        let mut connections = self.connections;
        connections.sort_by_key(|(name, _)| name.to_lowercase());
        out.extend(
            connections
                .into_iter()
                .map(|(_, id)| Top::Connection { id, depth }),
        );
    }
}

/// The tree.
#[derive(Default)]
pub struct MetaTree {
    top: Vec<Top>,
    collapsed_folders: HashSet<String>,
    expanded_connections: HashSet<ConnectionId>,
    expanded_objects: HashSet<(ConnectionId, ObjectPath)>,
    groups: HashMap<(ConnectionId, NodeKey), GroupState>,
    kinds: HashMap<(ConnectionId, Option<ObjectKind>), Rc<[ObjectKind]>>,
    rows: Vec<Row>,
}

impl MetaTree {
    /// An empty tree.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the saved connections: `(folder, name, id)`. Folders come first, then connections
    /// by name (case-insensitive).
    pub fn set_connections(&mut self, connections: Vec<(Vec<String>, String, ConnectionId)>) {
        let mut root = FolderNode::default();
        let mut ids = HashSet::new();
        for (folder, name, id) in connections {
            let mut node = &mut root;
            for segment in folder {
                node = node.folders.entry(segment).or_default();
            }
            node.connections.push((name, id));
            ids.insert(id);
        }
        let mut top = Vec::new();
        root.flatten("", 0, &mut top);
        self.top = top;
        // Forget what belonged to deleted connections.
        self.expanded_connections.retain(|c| ids.contains(c));
        self.expanded_objects.retain(|(c, _)| ids.contains(c));
        self.groups.retain(|(c, _), _| ids.contains(c));
        self.kinds.retain(|(c, _), _| ids.contains(c));
    }

    /// The rows of the last [`Self::rebuild`].
    #[must_use]
    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    /// The state of a group, if it was ever expanded.
    #[must_use]
    pub fn group_state(&self, connection: ConnectionId, key: &NodeKey) -> Option<&GroupState> {
        self.groups.get(&(connection, key.clone()))
    }

    fn kinds_of(
        &mut self,
        env: &mut dyn TreeEnv,
        connection: ConnectionId,
        parent: &ObjectPath,
    ) -> Rc<[ObjectKind]> {
        let key = (connection, parent.kind());
        if let Some(k) = self.kinds.get(&key) {
            return k.clone();
        }
        let kinds: Rc<[ObjectKind]> = env.child_kinds(connection, parent).into();
        // Only cache a real answer: a closed (or not yet bound) connection answers nothing
        // for now.
        if env.is_open(connection) && !kinds.is_empty() {
            self.kinds.insert(key, kinds.clone());
        }
        kinds
    }

    fn group(&mut self, connection: ConnectionId, key: NodeKey) -> &mut GroupState {
        self.groups
            .entry((connection, key.clone()))
            .or_insert_with(|| GroupState {
                group: Rc::new(GroupRef { connection, key }),
                expanded: false,
                children: None,
                loading: false,
                error: None,
            })
    }

    /// Recomputes the rows.
    pub fn rebuild(&mut self, env: &mut dyn TreeEnv) {
        let mut rows = Vec::new();
        let top = self.top.clone();
        let mut hidden_below: Option<usize> = None;
        for item in top {
            match item {
                Top::Folder { path, name, depth } => {
                    if hidden_below.is_some_and(|d| depth > d) {
                        continue;
                    }
                    hidden_below = None;
                    let expanded = !self.collapsed_folders.contains(&path);
                    if !expanded {
                        hidden_below = Some(depth);
                    }
                    rows.push(Row {
                        depth,
                        kind: RowKind::Folder {
                            path,
                            name,
                            expanded,
                        },
                    });
                }
                Top::Connection { id, depth } => {
                    if hidden_below.is_some_and(|d| depth > d) {
                        continue;
                    }
                    let expanded = self.expanded_connections.contains(&id);
                    rows.push(Row {
                        depth,
                        kind: RowKind::Connection { id, expanded },
                    });
                    if !expanded {
                        continue;
                    }
                    if !env.is_open(id) {
                        rows.push(Row {
                            depth: depth + 1,
                            kind: RowKind::NotConnected(id),
                        });
                        continue;
                    }
                    let kinds = self.kinds_of(env, id, &ObjectPath::root());
                    if kinds.is_empty() {
                        rows.push(Row {
                            depth: depth + 1,
                            kind: RowKind::NoKinds(id),
                        });
                    }
                    for kind in kinds.iter() {
                        self.push_group(
                            env,
                            id,
                            NodeKey::new(ObjectPath::root(), *kind),
                            depth + 1,
                            &mut rows,
                        );
                    }
                }
            }
        }
        self.rows = rows;
    }

    fn push_group(
        &mut self,
        env: &mut dyn TreeEnv,
        connection: ConnectionId,
        key: NodeKey,
        depth: usize,
        rows: &mut Vec<Row>,
    ) {
        let state = self.group(connection, key).clone();
        let group = state.group.clone();
        rows.push(Row {
            depth,
            kind: RowKind::Group {
                group: group.clone(),
                expanded: state.expanded,
            },
        });
        if !state.expanded {
            return;
        }
        let depth = depth + 1;
        let Some(children) = state.children.clone() else {
            rows.push(Row {
                depth,
                kind: match &state.error {
                    Some(message) => RowKind::Error {
                        group,
                        message: message.clone(),
                    },
                    None => RowKind::Loading(group),
                },
            });
            return;
        };
        for (index, item) in children.items.iter().enumerate() {
            let expandable = !self.kinds_of(env, connection, &item.path).is_empty();
            let expanded = expandable
                && self
                    .expanded_objects
                    .contains(&(connection, item.path.clone()));
            rows.push(Row {
                depth,
                kind: RowKind::Object {
                    group: group.clone(),
                    index,
                    expanded,
                    expandable,
                },
            });
            if expanded {
                let kinds = self.kinds_of(env, connection, &item.path);
                for kind in kinds.iter() {
                    self.push_group(
                        env,
                        connection,
                        NodeKey::new(item.path.clone(), *kind),
                        depth + 1,
                        rows,
                    );
                }
            }
        }
        if children.items.is_empty() && children.complete {
            rows.push(Row {
                depth,
                kind: RowKind::Empty(group.clone()),
            });
        }
        if let Some(message) = &state.error {
            rows.push(Row {
                depth,
                kind: RowKind::Error {
                    group: group.clone(),
                    message: message.clone(),
                },
            });
        }
        if !children.complete {
            rows.push(Row {
                depth,
                kind: if state.loading {
                    RowKind::LoadingMore(group.clone())
                } else {
                    RowKind::LoadMore(group.clone())
                },
            });
        }
        if children.hidden_by_filter > 0 {
            rows.push(Row {
                depth,
                kind: RowKind::Hidden {
                    group,
                    count: children.hidden_by_filter,
                },
            });
        }
    }

    /// The object of an object row.
    #[must_use]
    pub fn object(&self, row: &Row) -> Option<&ObjectInfo> {
        let RowKind::Object { group, index, .. } = &row.kind else {
            return None;
        };
        self.groups
            .get(&(group.connection, group.key.clone()))?
            .children
            .as_ref()?
            .items
            .get(*index)
    }

    /// The stable key of a row.
    #[must_use]
    pub fn key_of(&self, row: &Row) -> Option<RowKey> {
        Some(match &row.kind {
            RowKind::Folder { path, .. } => RowKey::Folder(path.clone()),
            RowKind::Connection { id, .. } => RowKey::Connection(*id),
            RowKind::NotConnected(id) | RowKind::NoKinds(id) => RowKey::ConnectionNote(*id),
            RowKind::Group { group, .. } => RowKey::Group(group.connection, group.key.clone()),
            RowKind::Object { group, .. } => {
                RowKey::Object(group.connection, self.object(row)?.path.clone())
            }
            RowKind::Loading(g) | RowKind::LoadingMore(g) => {
                RowKey::Status(g.connection, g.key.clone(), 0)
            }
            RowKind::Error { group, .. } => RowKey::Status(group.connection, group.key.clone(), 1),
            RowKind::LoadMore(g) => RowKey::Status(g.connection, g.key.clone(), 2),
            RowKind::Hidden { group, .. } => RowKey::Status(group.connection, group.key.clone(), 3),
            RowKind::Empty(g) => RowKey::Status(g.connection, g.key.clone(), 4),
        })
    }

    /// The index of the row with `key`.
    #[must_use]
    pub fn index_of(&self, key: &RowKey) -> Option<usize> {
        self.rows
            .iter()
            .position(|r| self.key_of(r).as_ref() == Some(key))
    }

    /// The connection a row belongs to.
    #[must_use]
    pub fn connection_of(row: &Row) -> Option<ConnectionId> {
        match &row.kind {
            RowKind::Folder { .. } => None,
            RowKind::Connection { id, .. } | RowKind::NotConnected(id) | RowKind::NoKinds(id) => {
                Some(*id)
            }
            RowKind::Group { group, .. }
            | RowKind::Object { group, .. }
            | RowKind::Loading(group)
            | RowKind::LoadingMore(group)
            | RowKind::Error { group, .. }
            | RowKind::LoadMore(group)
            | RowKind::Hidden { group, .. }
            | RowKind::Empty(group) => Some(group.connection),
        }
    }

    /// The group a row is or belongs to (an object's own group, a status row's group).
    #[must_use]
    pub fn group_of(row: &Row) -> Option<&Group> {
        match &row.kind {
            RowKind::Group { group, .. }
            | RowKind::Object { group, .. }
            | RowKind::Loading(group)
            | RowKind::LoadingMore(group)
            | RowKind::Error { group, .. }
            | RowKind::LoadMore(group)
            | RowKind::Hidden { group, .. }
            | RowKind::Empty(group) => Some(group),
            _ => None,
        }
    }

    /// Whether row `ix` can be expanded and is collapsed.
    #[must_use]
    pub fn can_expand(&self, ix: usize) -> bool {
        match self.rows.get(ix).map(|r| &r.kind) {
            Some(RowKind::Folder { expanded, .. })
            | Some(RowKind::Connection { expanded, .. })
            | Some(RowKind::Group { expanded, .. }) => !expanded,
            Some(RowKind::Object {
                expanded,
                expandable,
                ..
            }) => *expandable && !expanded,
            _ => false,
        }
    }

    /// Whether row `ix` is expanded.
    #[must_use]
    pub fn is_expanded(&self, ix: usize) -> bool {
        match self.rows.get(ix).map(|r| &r.kind) {
            Some(RowKind::Folder { expanded, .. })
            | Some(RowKind::Connection { expanded, .. })
            | Some(RowKind::Group { expanded, .. })
            | Some(RowKind::Object { expanded, .. }) => *expanded,
            _ => false,
        }
    }

    /// Expands row `ix`. Returns what the view must do (load, connect). Call
    /// [`Self::rebuild`] afterwards.
    pub fn expand(&mut self, ix: usize, env: &mut dyn TreeEnv) -> Vec<Effect> {
        let Some(row) = self.rows.get(ix).cloned() else {
            return Vec::new();
        };
        match &row.kind {
            RowKind::Folder { path, .. } => {
                self.collapsed_folders.remove(path);
                Vec::new()
            }
            RowKind::Connection { id, .. } => self.expand_connection(*id, env),
            RowKind::Group { group, .. } => {
                self.set_group_expanded(group.connection, &group.key, true)
            }
            RowKind::Object {
                group, expandable, ..
            } if *expandable => match self.object(&row).map(|o| o.path.clone()) {
                Some(path) => self.expand_object(group.connection, &path, env),
                None => Vec::new(),
            },
            _ => Vec::new(),
        }
    }

    /// Collapses row `ix`; a group that was loading gets [`Effect::Cancel`].
    pub fn collapse(&mut self, ix: usize) -> Vec<Effect> {
        let Some(row) = self.rows.get(ix).cloned() else {
            return Vec::new();
        };
        match &row.kind {
            RowKind::Folder { path, .. } => {
                self.collapsed_folders.insert(path.clone());
                Vec::new()
            }
            RowKind::Connection { id, .. } => {
                self.expanded_connections.remove(id);
                // Loads under a collapsed connection are dropped: nothing shows them.
                let mut out = Vec::new();
                for ((c, _), s) in &mut self.groups {
                    if *c == *id && s.loading {
                        s.loading = false;
                        out.push(Effect::Cancel(s.group.clone()));
                    }
                }
                out
            }
            RowKind::Group { group, .. } => {
                self.set_group_expanded(group.connection, &group.key, false)
            }
            RowKind::Object { group, .. } => {
                if let Some(path) = self.object(&row).map(|o| o.path.clone()) {
                    self.expanded_objects.remove(&(group.connection, path));
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// The index of the parent row of `ix` (the nearest row above with a smaller depth).
    #[must_use]
    pub fn parent_index(&self, ix: usize) -> Option<usize> {
        let depth = self.rows.get(ix)?.depth;
        (0..ix).rev().find(|i| self.rows[*i].depth < depth)
    }

    /// Expands a connection; a closed one asks to connect. A connection with a single root
    /// kind expands that group too.
    pub fn expand_connection(
        &mut self,
        connection: ConnectionId,
        env: &mut dyn TreeEnv,
    ) -> Vec<Effect> {
        self.expanded_connections.insert(connection);
        if !env.is_open(connection) {
            return vec![Effect::Connect(connection)];
        }
        let kinds = self.kinds_of(env, connection, &ObjectPath::root());
        if let [only] = kinds.as_ref() {
            return self.set_group_expanded(
                connection,
                &NodeKey::new(ObjectPath::root(), *only),
                true,
            );
        }
        Vec::new()
    }

    /// Expands an object; an object with a single child kind expands that group too.
    pub fn expand_object(
        &mut self,
        connection: ConnectionId,
        path: &ObjectPath,
        env: &mut dyn TreeEnv,
    ) -> Vec<Effect> {
        self.expanded_objects.insert((connection, path.clone()));
        let kinds = self.kinds_of(env, connection, path);
        if let [only] = kinds.as_ref() {
            return self.set_group_expanded(connection, &NodeKey::new(path.clone(), *only), true);
        }
        Vec::new()
    }

    /// Expands or collapses a group. Expanding an unloaded group starts a load; collapsing a
    /// loading one cancels it.
    pub fn set_group_expanded(
        &mut self,
        connection: ConnectionId,
        key: &NodeKey,
        expanded: bool,
    ) -> Vec<Effect> {
        let state = self.group(connection, key.clone());
        state.expanded = expanded;
        if expanded {
            if state.children.is_none() && !state.loading {
                state.loading = true;
                state.error = None;
                return vec![Effect::Load(state.group.clone())];
            }
        } else if state.loading {
            state.loading = false;
            return vec![Effect::Cancel(state.group.clone())];
        }
        Vec::new()
    }

    /// Marks a group loading again (refresh, load more). Returns the effect to run.
    pub fn reload(&mut self, connection: ConnectionId, key: &NodeKey, more: bool) -> Effect {
        let state = self.group(connection, key.clone());
        state.loading = true;
        state.error = None;
        if !more {
            state.children = None;
        }
        if more {
            Effect::LoadMore(state.group.clone())
        } else {
            Effect::Load(state.group.clone())
        }
    }

    /// A load answered.
    pub fn loaded(&mut self, connection: ConnectionId, key: &NodeKey, children: Arc<NodeChildren>) {
        let state = self.group(connection, key.clone());
        state.loading = false;
        state.error = None;
        state.children = Some(children);
    }

    /// A load failed (the loaded children, if any, stay).
    pub fn failed(&mut self, connection: ConnectionId, key: &NodeKey, message: &str) {
        let state = self.group(connection, key.clone());
        state.loading = false;
        state.error = Some(message.into());
    }

    /// A load was cancelled from elsewhere (the request went away): back to "not loaded".
    pub fn load_dropped(&mut self, connection: ConnectionId, key: &NodeKey) {
        if let Some(state) = self.groups.get_mut(&(connection, key.clone())) {
            state.loading = false;
        }
    }

    /// Cached data under `scope` was dropped by the core: the groups there forget their
    /// children; expanded ones are reloaded (the returned effects).
    pub fn invalidate(
        &mut self,
        connection: ConnectionId,
        scope: &InvalidationScope,
    ) -> Vec<Effect> {
        let covers = |key: &NodeKey| match scope {
            InvalidationScope::Connection => true,
            InvalidationScope::Subtree(path) => is_under(&key.parent, path),
            InvalidationScope::Node(node) => {
                key == node
                    || (key.parent.0.len() > node.parent.0.len()
                        && is_under(&key.parent, &node.parent)
                        && key.parent.0[node.parent.0.len()].kind == node.kind)
            }
            _ => true,
        };
        let mut out = Vec::new();
        for ((c, key), state) in &mut self.groups {
            if *c != connection || !covers(key) {
                continue;
            }
            state.children = None;
            state.error = None;
            if state.expanded {
                if !state.loading {
                    state.loading = true;
                    out.push(Effect::Load(state.group.clone()));
                }
            } else if state.loading {
                state.loading = false;
                out.push(Effect::Cancel(state.group.clone()));
            }
        }
        out
    }

    /// The session of a connection ended: everything loaded for it is forgotten (expansion of
    /// the connection row itself stays, so it shows "not connected"). Returns the loads to
    /// drop.
    pub fn forget_connection(&mut self, connection: ConnectionId) -> Vec<Effect> {
        let mut out = Vec::new();
        self.groups.retain(|(c, _), s| {
            if *c != connection {
                return true;
            }
            if s.loading {
                out.push(Effect::Cancel(s.group.clone()));
            }
            false
        });
        self.expanded_objects.retain(|(c, _)| *c != connection);
        self.kinds.retain(|(c, _), _| *c != connection);
        out
    }

    /// One step of revealing `path` of `connection`: expands the connection and every group
    /// and object on the way that is loaded, and returns the loads still needed, the row once
    /// it is there, or [`RevealStep::Missing`].
    pub fn reveal_step(
        &mut self,
        connection: ConnectionId,
        path: &ObjectPath,
        env: &mut dyn TreeEnv,
    ) -> RevealStep {
        let mut effects = Vec::new();
        if !self.expanded_connections.contains(&connection) {
            self.expanded_connections.insert(connection);
        }
        if !env.is_open(connection) {
            return RevealStep::Pending(vec![Effect::Connect(connection)]);
        }
        for depth in 0..path.0.len() {
            let parent = ObjectPath(path.0[..depth].to_vec());
            let segment = &path.0[depth];
            if depth > 0 {
                self.expanded_objects.insert((connection, parent.clone()));
            }
            let key = NodeKey::new(parent, segment.kind);
            effects.extend(self.set_group_expanded(connection, &key, true));
            let state = self.group(connection, key);
            if state.error.is_some() {
                return RevealStep::Missing;
            }
            let Some(children) = state.children.clone() else {
                return RevealStep::Pending(effects);
            };
            let here = ObjectPath(path.0[..=depth].to_vec());
            if !children.items.iter().any(|i| i.path == here) {
                return RevealStep::Missing;
            }
        }
        self.rebuild(env);
        RevealStep::Found(RowKey::Object(connection, path.clone()))
    }
}

/// `true` when `path` is `prefix` or lies under it.
fn is_under(path: &ObjectPath, prefix: &ObjectPath) -> bool {
    path.0.len() >= prefix.0.len() && path.0[..prefix.0.len()] == prefix.0[..]
}

/// The label of a row for tests and accessibility: folders end with `/`, groups are their
/// kind's plural, status rows say what they are.
#[must_use]
pub fn row_label(
    tree: &MetaTree,
    row: &Row,
    connection_name: &dyn Fn(ConnectionId) -> String,
) -> String {
    match &row.kind {
        RowKind::Folder { name, .. } => format!("{name}/"),
        RowKind::Connection { id, .. } => connection_name(*id),
        RowKind::NotConnected(_) => "Not connected".to_owned(),
        RowKind::NoKinds(_) => "No objects are listed for this engine".to_owned(),
        RowKind::Group { group, .. } => super::kind_plural(group.key.kind).to_owned(),
        RowKind::Object { .. } => tree
            .object(row)
            .map(|o| super::display_name(o.name()))
            .unwrap_or_default(),
        RowKind::Loading(_) => "Loading…".to_owned(),
        RowKind::LoadingMore(_) => "Loading more…".to_owned(),
        RowKind::Error { message, .. } => format!("Error: {message}"),
        RowKind::LoadMore(_) => "Load more".to_owned(),
        RowKind::Hidden { count, .. } => {
            if *count == 1 {
                "1 hidden by filter".to_owned()
            } else {
                format!("{count} hidden by filter")
            }
        }
        RowKind::Empty(_) => "(empty)".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake {
        open: bool,
    }

    impl TreeEnv for Fake {
        fn is_open(&self, _: ConnectionId) -> bool {
            self.open
        }
        fn child_kinds(&mut self, _: ConnectionId, parent: &ObjectPath) -> Vec<ObjectKind> {
            if !self.open {
                return Vec::new();
            }
            match parent.kind() {
                None => vec![ObjectKind::Schema, ObjectKind::Role],
                Some(ObjectKind::Schema) => vec![ObjectKind::Table, ObjectKind::View],
                _ => Vec::new(),
            }
        }
    }

    fn id() -> ConnectionId {
        ConnectionId::from_uuid(uuid::Uuid::from_bytes([7; 16]))
    }

    #[test]
    fn expanding_a_closed_connection_asks_to_connect() {
        let mut tree = MetaTree::new();
        let mut env = Fake { open: false };
        tree.set_connections(vec![(vec!["team".into()], "db".into(), id())]);
        tree.rebuild(&mut env);
        assert_eq!(tree.rows().len(), 2, "folder and connection");
        assert_eq!(tree.expand(1, &mut env), vec![Effect::Connect(id())]);
        tree.rebuild(&mut env);
        assert!(matches!(tree.rows()[2].kind, RowKind::NotConnected(_)));
        // Collapsing the folder hides the connection.
        assert!(tree.collapse(0).is_empty());
        tree.rebuild(&mut env);
        assert_eq!(tree.rows().len(), 1);
    }

    #[test]
    fn groups_load_on_expand_cancel_on_collapse_and_reload_on_invalidation() {
        let mut tree = MetaTree::new();
        let mut env = Fake { open: true };
        tree.set_connections(vec![(Vec::new(), "db".into(), id())]);
        tree.rebuild(&mut env);
        // Two root kinds: the groups stay collapsed, nothing loads.
        assert!(tree.expand(0, &mut env).is_empty());
        tree.rebuild(&mut env);
        assert_eq!(tree.rows().len(), 3, "connection, Schemas, Roles");
        let schemas = NodeKey::new(ObjectPath::root(), ObjectKind::Schema);
        let ix = tree
            .index_of(&RowKey::Group(id(), schemas.clone()))
            .unwrap_or_else(|| panic!("schemas group"));
        assert!(matches!(&tree.expand(ix, &mut env)[..], [Effect::Load(g)] if g.key == schemas));
        tree.rebuild(&mut env);
        assert!(matches!(tree.rows()[ix + 1].kind, RowKind::Loading(_)));
        // Collapsing while loading cancels; expanding again loads again.
        assert!(matches!(&tree.collapse(ix)[..], [Effect::Cancel(_)]));
        assert!(matches!(&tree.expand(ix, &mut env)[..], [Effect::Load(_)]));
        // A failure is shown on the group.
        tree.failed(id(), &schemas, "the session is closed");
        tree.rebuild(&mut env);
        assert!(
            matches!(&tree.rows()[ix + 1].kind, RowKind::Error { message, .. } if &**message == "the session is closed")
        );
        // Invalidation reloads expanded groups under the scope only.
        let effects = tree.invalidate(id(), &InvalidationScope::Node(schemas.clone()));
        assert!(matches!(&effects[..], [Effect::Load(g)] if g.key == schemas));
        let roles = NodeKey::new(ObjectPath::root(), ObjectKind::Role);
        assert!(
            tree.invalidate(
                id(),
                &InvalidationScope::Subtree(ObjectPath::root().child(ObjectKind::Role, "r"))
            )
            .is_empty()
        );
        assert!(tree.group_state(id(), &roles).is_some_and(|s| !s.expanded));
        // A closed session forgets everything of the connection and drops its loads.
        assert_eq!(tree.forget_connection(id()).len(), 1);
        env.open = false;
        tree.rebuild(&mut env);
        assert_eq!(tree.rows().len(), 2, "connection and \"not connected\"");
    }
}
