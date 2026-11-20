//! Database navigator dock: the saved connections in their folders (F-CON-02), with the type
//! color, the type and read-only badges, a security badge and an "open" marker (ADR-0007 §4),
//! and under each open connection its objects (F-NAV-03, P4-B).
//!
//! - **Lazy and async.** Expanding a group ("Tables") asks the metadata service for it
//!   (`children`) and shows a "Loading…" row; the answer is awaited in a GPUI task (I6).
//!   Collapsing drops the task, and with it the request, which cancels the driver call. A group
//!   the server has more of ends with "Load more" (`load_more`); "N hidden by filter" says what
//!   the navigator filters hid; errors are shown on the group (the service's presentable text,
//!   no secret).
//! - **Declared kinds only** (I8): the groups under an object are the service's `child_kinds`.
//! - **Virtualized** (I6): rows point into the core's loaded children and only the visible ones
//!   are rendered (`uniform_list`).
//! - **In sync**: `MetadataEvent::Invalidated` (refresh, filters changed, session closed) drops
//!   what the tree shows under its scope and reloads the expanded groups there.
//! - **Keyboard**: up / down / home / end / page up / page down move, right expands (or goes
//!   to the first child), left collapses (or goes to the parent), Enter toggles, retries a
//!   failed group, loads more or opens a table in the object editor. The connection and
//!   metadata actions apply to the selected row through their key bindings and the command
//!   palette.

use std::collections::HashMap;
use std::ops::Range;

use gpui_kit::base::actions::{
    Confirm, SelectDown, SelectFirst, SelectLast, SelectLeft, SelectPageDown, SelectPageUp,
    SelectRight, SelectUp,
};
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenu};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use ninesql_core::driver_api::metadata::{ObjectInfo, ObjectPath};
use ninesql_core::{
    ConnectionId, InvalidationReason, MetadataError, MetadataEvent, MetadataService, NodeChildren,
    NodeKey,
};

use crate::actions::{
    ConfigureNavigatorFilters, ConnectConnection, CopyObjectDdl, FindObject, GenerateDeleteSql,
    GenerateInsertSql, GenerateSelectSql, GenerateUpdateSql, ImportFromDbeaver, LoadMoreObjects,
    NewConnection, OpenObjectEditor, OpenTableData, RefreshNavigatorNode, ShowObjectDdl,
};
use crate::browse::generate::{Template, TemplateAllowance};
use crate::browse::tree::{Effect, Group, MetaTree, RevealStep, Row, RowKey, RowKind, TreeEnv};
use crate::browse::{is_table_like, kind_plural};
use crate::connections::hub::{ConnectionHub, MetadataUpdate, hub_or_install_empty};
use crate::connections::visuals::{ConnectionBadges, badges};
use crate::import::enable::disabled_text;
use crate::import::model::disabled_kind_label;
use crate::keymap::EffectiveKeymap;

/// Key context of the navigator (its keyboard navigation bindings).
pub const NAVIGATOR_CONTEXT: &str = "Navigator";

/// Height of one row.
const ROW_HEIGHT: Pixels = px(24.);
/// Rows moved by page up / page down.
const PAGE_ROWS: usize = 20;

/// Binds the navigator's keyboard navigation (gpui-component's list actions, in
/// [`NAVIGATOR_CONTEXT`]). Called by `app::init`.
pub fn bind_keys(cx: &mut App) {
    let ctx = Some(NAVIGATOR_CONTEXT);
    cx.bind_keys([
        KeyBinding::new("up", SelectUp, ctx),
        KeyBinding::new("down", SelectDown, ctx),
        KeyBinding::new("left", SelectLeft, ctx),
        KeyBinding::new("right", SelectRight, ctx),
        KeyBinding::new("home", SelectFirst, ctx),
        KeyBinding::new("end", SelectLast, ctx),
        KeyBinding::new("pageup", SelectPageUp, ctx),
        KeyBinding::new("pagedown", SelectPageDown, ctx),
        KeyBinding::new("enter", Confirm { secondary: false }, ctx),
    ]);
}

/// What the navigator asks of its workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NavigatorEvent {
    /// Open a session on a connection (a closed connection was expanded).
    Connect(ConnectionId),
    /// Open an object in the object editor.
    OpenObject {
        /// The connection.
        connection: ConnectionId,
        /// The object.
        path: ObjectPath,
    },
    /// A reveal ended without finding the object (filtered, not loaded, gone).
    RevealFailed(String),
}

impl EventEmitter<NavigatorEvent> for Navigator {}

/// What is selected, for the actions.
#[derive(Debug, Clone)]
pub enum Selection {
    /// A connection row (or its note row).
    Connection(ConnectionId),
    /// A group, or a status row of it.
    Group(ConnectionId, NodeKey),
    /// An object.
    Object(ConnectionId, ObjectInfo),
}

impl Selection {
    /// The connection of the selection.
    #[must_use]
    pub fn connection(&self) -> ConnectionId {
        match self {
            Self::Connection(c) | Self::Group(c, _) | Self::Object(c, _) => *c,
        }
    }
}

/// The short label of a disabled connection in its row ("disabled: no driver") and the full
/// text of its tooltip; `None` when enabled.
#[must_use]
pub fn row_disabled_label(c: &ninesql_core::Connection) -> Option<(String, SharedString)> {
    let full = disabled_text(c.enabled, c.disabled_reason.as_ref())?;
    let short = match &c.disabled_reason {
        Some(r) => format!("disabled: {}", disabled_kind_label(r.kind)),
        None => "disabled".to_owned(),
    };
    Some((short, full.into()))
}

struct Env<'a> {
    hub: &'a ConnectionHub,
    metadata: Option<MetadataService>,
}

impl TreeEnv for Env<'_> {
    fn is_open(&self, connection: ConnectionId) -> bool {
        self.hub.is_open(connection)
    }

    fn child_kinds(
        &mut self,
        connection: ConnectionId,
        parent: &ObjectPath,
    ) -> Vec<ninesql_core::driver_api::ObjectKind> {
        if !self.hub.is_open(connection) {
            return Vec::new();
        }
        self.metadata
            .as_ref()
            .and_then(|m| m.child_kinds(connection, parent).ok())
            .unwrap_or_default()
    }
}

/// A pending "reveal in navigator".
struct Reveal {
    connection: ConnectionId,
    path: ObjectPath,
    /// Waits for the connection's session to open.
    waits_for_session: bool,
}

/// The navigator view.
pub struct Navigator {
    hub: Entity<ConnectionHub>,
    tree: MetaTree,
    selected: Option<RowKey>,
    selected_ix: Option<usize>,
    focus: FocusHandle,
    scroll: UniformListScrollHandle,
    loads: HashMap<(ConnectionId, NodeKey), Task<()>>,
    reveal: Option<Reveal>,
    _subscriptions: Vec<Subscription>,
}

impl Navigator {
    /// The navigator of the application's connections.
    pub fn new(cx: &mut Context<Self>) -> Self {
        let hub = hub_or_install_empty(cx);
        let subscriptions = vec![
            cx.observe(&hub, |this: &mut Self, hub, cx| {
                this.rebuild(cx);
                let resume = this
                    .reveal
                    .as_ref()
                    .is_some_and(|r| r.waits_for_session && hub.read(cx).is_open(r.connection));
                if resume {
                    this.advance_reveal(cx);
                }
            }),
            cx.subscribe(&hub, |this: &mut Self, _, event: &MetadataUpdate, cx| {
                this.on_metadata_event(&event.0, cx);
            }),
        ];
        let mut this = Self {
            hub,
            tree: MetaTree::new(),
            selected: None,
            selected_ix: None,
            focus: cx.focus_handle(),
            scroll: UniformListScrollHandle::new(),
            loads: HashMap::new(),
            reveal: None,
            _subscriptions: subscriptions,
        };
        this.rebuild(cx);
        this
    }

    fn with_env<R>(&mut self, cx: &App, f: impl FnOnce(&mut MetaTree, &mut Env<'_>) -> R) -> R {
        let hub = self.hub.read(cx);
        let mut env = Env {
            hub,
            metadata: hub.metadata(),
        };
        f(&mut self.tree, &mut env)
    }

    /// Rebuilds the rows from the hub and the loaded metadata, keeping the selection.
    pub fn rebuild(&mut self, cx: &mut Context<Self>) {
        let connections: Vec<_> = self
            .hub
            .read(cx)
            .connections()
            .iter()
            .map(|c| (c.folder.clone(), c.name.clone(), c.id))
            .collect();
        self.with_env(cx, |tree, env| {
            tree.set_connections(connections);
            tree.rebuild(env);
        });
        self.selected_ix = self.selected.as_ref().and_then(|k| self.tree.index_of(k));
        if self.selected_ix.is_none() {
            self.selected = None;
        }
        cx.notify();
    }

    /// Whether there is no connection.
    #[must_use]
    pub fn is_empty(&self, _cx: &App) -> bool {
        self.tree.rows().is_empty()
    }

    /// The tree model.
    #[must_use]
    pub fn tree(&self) -> &MetaTree {
        &self.tree
    }

    /// The selected row's index.
    #[must_use]
    pub fn selected_index(&self) -> Option<usize> {
        self.selected_ix
    }

    /// The selected connection, if a connection row is selected.
    #[must_use]
    pub fn selected_connection(&self, _cx: &App) -> Option<ConnectionId> {
        match self.selected.as_ref()? {
            RowKey::Connection(id) => Some(*id),
            _ => None,
        }
    }

    /// What is selected (connection, group or object).
    #[must_use]
    pub fn selection(&self) -> Option<Selection> {
        let row = self.tree.rows().get(self.selected_ix?)?;
        Some(match &row.kind {
            RowKind::Folder { .. } => return None,
            RowKind::Connection { id, .. } | RowKind::NotConnected(id) | RowKind::NoKinds(id) => {
                Selection::Connection(*id)
            }
            RowKind::Object { group, .. } => {
                Selection::Object(group.connection, self.tree.object(row)?.clone())
            }
            _ => {
                let group = MetaTree::group_of(row)?;
                Selection::Group(group.connection, group.key.clone())
            }
        })
    }

    /// Selects a row by index.
    pub fn select_index(&mut self, ix: Option<usize>, cx: &mut Context<Self>) {
        self.selected_ix = ix.filter(|i| *i < self.tree.rows().len());
        self.selected = self
            .selected_ix
            .and_then(|i| self.tree.rows().get(i))
            .and_then(|r| self.tree.key_of(r));
        if let Some(ix) = self.selected_ix {
            self.scroll.scroll_to_item(ix, ScrollStrategy::Nearest);
        }
        cx.notify();
    }

    /// Selects a connection row.
    pub fn select_connection(&mut self, id: ConnectionId, cx: &mut Context<Self>) {
        let ix = self.tree.index_of(&RowKey::Connection(id));
        if ix.is_some() {
            self.select_index(ix, cx);
        }
    }

    /// Row labels in display order with their depth, for tests: folders end with `/`, groups
    /// are their kind's plural, status rows say what they are.
    #[must_use]
    pub fn rows(&self, cx: &App) -> Vec<(usize, String)> {
        let hub = self.hub.read(cx);
        let name = |id: ConnectionId| {
            hub.connection(id)
                .map(|c| c.name.clone())
                .unwrap_or_default()
        };
        self.tree
            .rows()
            .iter()
            .map(|r| {
                (
                    r.depth,
                    crate::browse::tree::row_label(&self.tree, r, &name),
                )
            })
            .collect()
    }

    /// Moves keyboard focus to the tree.
    pub fn focus_tree(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.focus.focus(window, cx);
        if self.selected_ix.is_none() && !self.tree.rows().is_empty() {
            self.select_index(Some(0), cx);
        }
    }

    // -- Expansion and loads ------------------------------------------------------------------

    /// Expands row `ix` (loads its group, connects a closed connection).
    pub fn expand(&mut self, ix: usize, cx: &mut Context<Self>) {
        let effects = self.with_env(cx, |tree, env| tree.expand(ix, env));
        self.run(effects, cx);
        self.rebuild(cx);
    }

    /// Collapses row `ix` (drops the loads under it).
    pub fn collapse(&mut self, ix: usize, cx: &mut Context<Self>) {
        let effects = self.tree.collapse(ix);
        self.run(effects, cx);
        self.rebuild(cx);
    }

    /// Expands or collapses row `ix`.
    pub fn toggle(&mut self, ix: usize, cx: &mut Context<Self>) {
        if self.tree.is_expanded(ix) {
            self.collapse(ix, cx);
        } else if self.tree.can_expand(ix) {
            self.expand(ix, cx);
        }
    }

    /// Enter on row `ix`: "Load more" loads more, an error row retries, a table-like object
    /// opens in the object editor, other rows toggle.
    pub fn activate(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(row) = self.tree.rows().get(ix).cloned() else {
            return;
        };
        match &row.kind {
            RowKind::LoadMore(group) => {
                let group = group.clone();
                self.load_more(&group, cx);
            }
            RowKind::Error { group, .. } => {
                let group = group.clone();
                self.refresh_group(group.connection, &group.key, cx);
            }
            RowKind::Object { group, .. } => {
                let connection = group.connection;
                match self.tree.object(&row) {
                    Some(o) if o.path.kind().is_some_and(is_table_like) => {
                        let path = o.path.clone();
                        cx.emit(NavigatorEvent::OpenObject { connection, path });
                    }
                    _ => self.toggle(ix, cx),
                }
            }
            _ => self.toggle(ix, cx),
        }
    }

    fn run(&mut self, effects: Vec<Effect>, cx: &mut Context<Self>) {
        for effect in effects {
            match effect {
                Effect::Load(g) => self.start_load(&g, false, cx),
                Effect::LoadMore(g) => self.start_load(&g, true, cx),
                Effect::Cancel(g) => {
                    // Dropping the task drops the request: the service cancels the driver
                    // call when no one else waits for that node.
                    self.loads.remove(&(g.connection, g.key.clone()));
                }
                Effect::Connect(id) => cx.emit(NavigatorEvent::Connect(id)),
            }
        }
    }

    fn start_load(&mut self, group: &Group, more: bool, cx: &mut Context<Self>) {
        let connection = group.connection;
        let key = group.key.clone();
        let Some(service) = self.hub.read(cx).metadata() else {
            self.tree.failed(
                connection,
                &key,
                "metadata is not available: the driver runtime did not start",
            );
            return;
        };
        if !more && let Some(children) = service.cached_children(connection, &key) {
            self.loads.remove(&(connection, key.clone()));
            self.tree.loaded(connection, &key, children);
            return;
        }
        let request = if more {
            service.load_more(connection, key.clone())
        } else {
            service.children(connection, key.clone())
        };
        self.await_load(connection, key, request, cx);
    }

    fn await_load(
        &mut self,
        connection: ConnectionId,
        key: NodeKey,
        request: ninesql_core::MetadataRequest<std::sync::Arc<NodeChildren>>,
        cx: &mut Context<Self>,
    ) {
        let task_key = key.clone();
        let task = cx.spawn(async move |this, cx| {
            let result = request.await;
            let _ = this.update(cx, |nav, cx| {
                nav.on_loaded(connection, task_key, result, cx)
            });
        });
        self.loads.insert((connection, key), task);
    }

    fn on_loaded(
        &mut self,
        connection: ConnectionId,
        key: NodeKey,
        result: Result<std::sync::Arc<NodeChildren>, MetadataError>,
        cx: &mut Context<Self>,
    ) {
        self.loads.remove(&(connection, key.clone()));
        match result {
            Ok(children) => self.tree.loaded(connection, &key, children),
            Err(e) => self.tree.failed(
                connection,
                &key,
                &crate::browse::display_message(&e.to_string()),
            ),
        }
        self.rebuild(cx);
        self.advance_reveal(cx);
    }

    /// Whether a load is in flight for a group (tests).
    #[must_use]
    pub fn is_loading(&self, connection: ConnectionId, key: &NodeKey) -> bool {
        self.loads.contains_key(&(connection, key.clone()))
    }

    /// Number of loads in flight (tests).
    #[must_use]
    pub fn loads_in_flight(&self) -> usize {
        self.loads.len()
    }

    fn load_more(&mut self, group: &Group, cx: &mut Context<Self>) {
        let effect = self.tree.reload(group.connection, &group.key, true);
        self.run(vec![effect], cx);
        self.rebuild(cx);
    }

    fn refresh_group(&mut self, connection: ConnectionId, key: &NodeKey, cx: &mut Context<Self>) {
        let Some(service) = self.hub.read(cx).metadata() else {
            return;
        };
        let _ = self.tree.reload(connection, key, false);
        let request = service.refresh(connection, key.clone());
        self.await_load(connection, key.clone(), request, cx);
        self.rebuild(cx);
    }

    /// "Refresh navigator node": reloads the selected group (an object's own group, a status
    /// row's group) or everything of the selected connection. Returns false when nothing
    /// applies.
    pub fn refresh_selected(&mut self, cx: &mut Context<Self>) -> bool {
        match self.selection() {
            Some(Selection::Group(connection, key)) => {
                self.refresh_group(connection, &key, cx);
                true
            }
            Some(Selection::Object(connection, object)) => {
                let Some((leaf, parents)) = object.path.0.split_last() else {
                    return false;
                };
                let key = NodeKey::new(ObjectPath(parents.to_vec()), leaf.kind);
                self.refresh_group(connection, &key, cx);
                true
            }
            Some(Selection::Connection(connection)) => {
                let Some(service) = self.hub.read(cx).metadata() else {
                    return false;
                };
                if !self.hub.read(cx).is_open(connection) {
                    return false;
                }
                // The Invalidated event reloads the expanded groups.
                service.invalidate(connection, &ObjectPath::root());
                true
            }
            None => false,
        }
    }

    /// "Load more objects" on the selected group. Returns false when it has nothing more.
    pub fn load_more_selected(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(ix) = self.selected_ix else {
            return false;
        };
        let Some(group) = self
            .tree
            .rows()
            .get(ix)
            .and_then(MetaTree::group_of)
            .cloned()
        else {
            return false;
        };
        let state = self.tree.group_state(group.connection, &group.key);
        let incomplete = state
            .and_then(|s| s.children.as_ref())
            .is_some_and(|c| !c.complete);
        if !incomplete || state.is_some_and(|s| s.loading) {
            return false;
        }
        self.load_more(&group, cx);
        true
    }

    // -- Events -------------------------------------------------------------------------------

    fn on_metadata_event(&mut self, event: &MetadataEvent, cx: &mut Context<Self>) {
        match event {
            MetadataEvent::Invalidated {
                connection,
                scope,
                reason,
            } => {
                let effects = if *reason == InvalidationReason::SessionClosed {
                    self.tree.forget_connection(*connection)
                } else {
                    self.tree.invalidate(*connection, scope)
                };
                self.run(effects, cx);
                self.rebuild(cx);
            }
            MetadataEvent::NodeLoaded {
                connection,
                key,
                cached: true,
                ..
            } => {
                // Loaded elsewhere (another window, a prefetch): show it if this tree waits
                // for it without a request of its own.
                let waiting = self
                    .tree
                    .group_state(*connection, key)
                    .is_some_and(|s| s.expanded && s.children.is_none())
                    && !self.loads.contains_key(&(*connection, key.clone()));
                if waiting
                    && let Some(children) = self
                        .hub
                        .read(cx)
                        .metadata()
                        .and_then(|m| m.cached_children(*connection, key))
                {
                    self.tree.loaded(*connection, key, children);
                    self.rebuild(cx);
                }
            }
            _ => {}
        }
    }

    // -- Reveal (F-NAV-05) --------------------------------------------------------------------

    /// Reveals an object: expands the path to it, loading the groups on the way, then selects
    /// it. A closed connection is connected first.
    pub fn reveal(&mut self, connection: ConnectionId, path: ObjectPath, cx: &mut Context<Self>) {
        self.reveal = Some(Reveal {
            connection,
            path,
            waits_for_session: false,
        });
        self.advance_reveal(cx);
    }

    /// Whether a reveal is going on (tests).
    #[must_use]
    pub fn is_revealing(&self) -> bool {
        self.reveal.is_some()
    }

    fn advance_reveal(&mut self, cx: &mut Context<Self>) {
        // Each round goes at least one level deeper or stops; bounded by the path length.
        for _ in 0..64 {
            let Some(mut reveal) = self.reveal.take() else {
                return;
            };
            let step = self.with_env(cx, |tree, env| {
                tree.reveal_step(reveal.connection, &reveal.path, env)
            });
            match step {
                RevealStep::Found(key) => {
                    self.rebuild(cx);
                    let ix = self.tree.index_of(&key);
                    self.select_index(ix, cx);
                    return;
                }
                RevealStep::Missing => {
                    self.rebuild(cx);
                    cx.emit(NavigatorEvent::RevealFailed(format!(
                        "\"{}\" is not in the navigator: it may be hidden by a filter, not loaded yet (\"Load more\"), or gone.",
                        crate::browse::display_path(&reveal.path)
                    )));
                    return;
                }
                RevealStep::Pending(effects) => {
                    reveal.waits_for_session =
                        effects.iter().any(|e| matches!(e, Effect::Connect(_)));
                    let connection = reveal.connection;
                    let had_effects = !effects.is_empty();
                    let waits_for_session = reveal.waits_for_session;
                    self.reveal = Some(reveal);
                    self.run(effects, cx);
                    self.rebuild(cx);
                    if waits_for_session || self.loads.keys().any(|(c, _)| *c == connection) {
                        // Picked up again when the session opens or the loads answer.
                        return;
                    }
                    if !had_effects {
                        // Nothing to wait for and nothing started: give up.
                        self.reveal = None;
                        return;
                    }
                    // Loaded from the cache at once: next level.
                }
            }
        }
        self.reveal = None;
    }

    // -- Keyboard ------------------------------------------------------------------------------

    fn on_up(&mut self, _: &SelectUp, _: &mut Window, cx: &mut Context<Self>) {
        let ix = match self.selected_ix {
            Some(0) | None => 0,
            Some(i) => i - 1,
        };
        if !self.tree.rows().is_empty() {
            self.select_index(Some(ix), cx);
        }
    }

    fn on_down(&mut self, _: &SelectDown, _: &mut Window, cx: &mut Context<Self>) {
        let len = self.tree.rows().len();
        if len == 0 {
            return;
        }
        let ix = self.selected_ix.map_or(0, |i| (i + 1).min(len - 1));
        self.select_index(Some(ix), cx);
    }

    fn on_first(&mut self, _: &SelectFirst, _: &mut Window, cx: &mut Context<Self>) {
        if !self.tree.rows().is_empty() {
            self.select_index(Some(0), cx);
        }
    }

    fn on_last(&mut self, _: &SelectLast, _: &mut Window, cx: &mut Context<Self>) {
        let len = self.tree.rows().len();
        if len > 0 {
            self.select_index(Some(len - 1), cx);
        }
    }

    fn on_page_up(&mut self, _: &SelectPageUp, _: &mut Window, cx: &mut Context<Self>) {
        if !self.tree.rows().is_empty() {
            let ix = self.selected_ix.unwrap_or(0).saturating_sub(PAGE_ROWS);
            self.select_index(Some(ix), cx);
        }
    }

    fn on_page_down(&mut self, _: &SelectPageDown, _: &mut Window, cx: &mut Context<Self>) {
        let len = self.tree.rows().len();
        if len > 0 {
            let ix = (self.selected_ix.unwrap_or(0) + PAGE_ROWS).min(len - 1);
            self.select_index(Some(ix), cx);
        }
    }

    fn on_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.selected_ix else {
            return;
        };
        if self.tree.can_expand(ix) {
            self.expand(ix, cx);
        } else if self.tree.is_expanded(ix) && ix + 1 < self.tree.rows().len() {
            self.select_index(Some(ix + 1), cx);
        }
    }

    fn on_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.selected_ix else {
            return;
        };
        if self.tree.is_expanded(ix) {
            self.collapse(ix, cx);
        } else if let Some(parent) = self.tree.parent_index(ix) {
            self.select_index(Some(parent), cx);
        }
    }

    fn on_confirm(&mut self, _: &Confirm, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.selected_ix {
            self.activate(ix, cx);
        }
    }

    // -- Rendering -----------------------------------------------------------------------------

    fn render_row(&self, ix: usize, row: &Row, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let selected = self.selected_ix == Some(ix);
        let muted = theme.muted_foreground;
        let chevron = |expanded: bool, expandable: bool| {
            div()
                .w(px(12.))
                .flex_none()
                .text_xs()
                .text_color(muted)
                .child(match (expandable, expanded) {
                    (false, _) => "",
                    (true, true) => "▾",
                    (true, false) => "▸",
                })
        };
        let note = |text: String| div().text_xs().text_color(muted).child(text);
        let content: AnyElement = match &row.kind {
            RowKind::Folder { name, expanded, .. } => h_flex()
                .gap_1()
                .child(chevron(*expanded, true))
                .child(
                    div()
                        .text_color(muted)
                        .child(crate::browse::display_name(name)),
                )
                .into_any_element(),
            RowKind::Connection { id, expanded } => h_flex()
                .gap_1()
                .child(chevron(*expanded, true))
                .child(self.render_connection(*id, cx))
                .into_any_element(),
            RowKind::NotConnected(_) => {
                note(not_connected_text(cx.try_global::<EffectiveKeymap>())).into_any_element()
            }
            RowKind::NoKinds(_) => {
                note("No objects are listed for this engine".to_owned()).into_any_element()
            }
            RowKind::Group { group, expanded } => h_flex()
                .gap_1()
                .child(chevron(*expanded, true))
                .child(div().child(kind_plural(group.key.kind)))
                .when_some(
                    self.tree
                        .group_state(group.connection, &group.key)
                        .and_then(|s| s.children.as_ref())
                        .map(|c| c.items.len()),
                    |el, n| el.child(div().text_xs().text_color(muted).child(format!("{n}"))),
                )
                .into_any_element(),
            RowKind::Object {
                expanded,
                expandable,
                ..
            } => {
                let object = self.tree.object(row);
                let name = object
                    .map(|o| crate::browse::display_name(o.name()))
                    .unwrap_or_default();
                let comment = object.and_then(|o| o.comment.clone());
                h_flex()
                    .gap_1()
                    .child(chevron(*expanded, *expandable))
                    .child(div().child(name))
                    .when_some(comment, |el, c| {
                        el.child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .overflow_hidden()
                                .child(crate::browse::display_name(&c)),
                        )
                    })
                    .into_any_element()
            }
            RowKind::Loading(_) => note("Loading…".to_owned()).into_any_element(),
            RowKind::LoadingMore(_) => note("Loading more…".to_owned()).into_any_element(),
            RowKind::Error { message, .. } => div()
                .text_xs()
                .text_color(theme.danger)
                .child(format!("{message} (Enter to retry)"))
                .into_any_element(),
            RowKind::LoadMore(_) => div()
                .text_xs()
                .text_color(theme.link)
                .child("Load more")
                .into_any_element(),
            RowKind::Hidden { count, .. } => note(if *count == 1 {
                "1 hidden by filter".to_owned()
            } else {
                format!("{count} hidden by filter")
            })
            .into_any_element(),
            RowKind::Empty(_) => note("(empty)".to_owned()).into_any_element(),
        };
        #[allow(clippy::cast_precision_loss)] // A tree depth is small.
        let indent = 8. + 14. * row.depth as f32;
        div()
            .id(("nav-row", ix))
            .h(ROW_HEIGHT)
            .w_full()
            .flex()
            .items_center()
            .pl(px(indent))
            .pr_2()
            .when(selected, |el| {
                el.bg(theme.list_active)
                    .border_l_2()
                    .border_color(theme.ring)
            })
            .hover(|s| s.bg(theme.list_hover))
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                this.focus.focus(window, cx);
                this.select_index(Some(ix), cx);
                let is_object = matches!(
                    this.tree.rows().get(ix).map(|r| &r.kind),
                    Some(RowKind::Object { .. })
                );
                // A double click opens a table; a single click toggles.
                if event.click_count() >= 2 || !is_object {
                    this.activate(ix, cx);
                } else {
                    this.toggle(ix, cx);
                }
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, _, window, cx| {
                    this.focus.focus(window, cx);
                    this.select_index(Some(ix), cx);
                }),
            )
            .child(content)
            .context_menu(self.row_menu(row))
            .into_any_element()
    }

    /// The context menu of a row: the actions that apply to it (the same actions as the key
    /// bindings and the palette, dispatched from the navigator).
    fn row_menu(
        &self,
        row: &Row,
    ) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static {
        let focus = self.focus.clone();
        let hub = self.hub.clone();
        let object = self
            .tree
            .object(row)
            .map(|o| (MetaTree::connection_of(row), o.path.clone()));
        let incomplete = MetaTree::group_of(row)
            .and_then(|g| self.tree.group_state(g.connection, &g.key))
            .and_then(|s| s.children.as_ref())
            .is_some_and(|c| !c.complete);
        let is_folder = matches!(row.kind, RowKind::Folder { .. });
        move |menu, _window, cx| {
            if is_folder {
                return menu;
            }
            let mut menu = menu.action_context(focus.clone());
            if let Some((connection, path)) = &object {
                for entry in object_menu_for(hub.read(cx), *connection, path) {
                    menu = match entry {
                        Some((label, action)) => menu.menu(label, action),
                        None => menu.separator(),
                    };
                }
            }
            if incomplete {
                menu = menu.menu("Load more objects", Box::new(LoadMoreObjects));
            }
            menu.menu("Refresh navigator node", Box::new(RefreshNavigatorNode))
                .menu(
                    "Configure navigator filters",
                    Box::new(ConfigureNavigatorFilters),
                )
                .menu("Find object", Box::new(FindObject))
        }
    }

    fn render_connection(&self, id: ConnectionId, cx: &App) -> AnyElement {
        let hub = self.hub.read(cx);
        let Some(c) = hub.connection(id) else {
            return div().into_any_element();
        };
        let sessions = hub.sessions(id);
        let b = ConnectionBadges::of(c, hub.types(), sessions);
        let open = !sessions.is_empty();
        let key = id.to_string();
        h_flex()
            .gap_2()
            .child(div().w(px(4.)).h(px(16.)).rounded_sm().bg(b.color))
            .child(crate::browse::display_name(&c.name))
            .when(open, |el| {
                el.child(
                    div()
                        .id(SharedString::from(format!("open-{key}")))
                        .text_xs()
                        .text_color(cx.theme().success)
                        .child("connected"),
                )
            })
            .when_some(row_disabled_label(c), |el, (short, full)| {
                // The reason (imported disabled, P2-B) in the row and its tooltip; "Enable
                // connection" shows it in full too.
                el.child(
                    div()
                        .id(SharedString::from(format!("disabled-{key}")))
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .tooltip(move |window, cx| Tooltip::new(full.clone()).build(window, cx))
                        .child(short),
                )
            })
            .child(badges(&key, &b, false, cx))
            .into_any_element()
    }
}

/// How the hints of the navigator name `action`: its label with its first binding in effect
/// (`"New connection" (Ctrl+Alt+N)`), overrides of `[keybindings]` included, or the label alone
/// when the action is unbound (review L8: never a hard-coded binding).
fn action_hint(label: &str, action: &dyn Action, keymap: Option<&EffectiveKeymap>) -> String {
    match keymap.and_then(|k| k.display_for(action.name())) {
        Some(keys) => format!("\"{label}\" ({keys})"),
        None => format!("\"{label}\""),
    }
}

/// The note under a connection that is not open: the binding of "Connect" in effect, or the
/// context menu when it is unbound (review L8).
pub(crate) fn not_connected_text(keymap: Option<&EffectiveKeymap>) -> String {
    match keymap.and_then(|k| k.display_for(ConnectConnection.name())) {
        Some(keys) => format!("Not connected (\"Connect\": {keys})"),
        None => "Not connected (\"Connect\" in the context menu)".to_owned(),
    }
}

/// The text of the empty navigator.
pub(crate) fn no_connections_text(keymap: Option<&EffectiveKeymap>) -> String {
    format!(
        "No connections yet. Use {} or {}.",
        action_hint("New connection", &NewConnection, keymap),
        action_hint("Import from DBeaver", &ImportFromDbeaver, keymap)
    )
}

/// One entry of a context menu: its label and action, or `None` for a separator.
pub(crate) type MenuEntry = Option<(String, Box<dyn Action>)>;

/// The object part of the context menu of the object at `path` of `connection`, as the menu
/// shows it now: the "Generate SQL" templates from the connection's first open session
/// (read-only, declared row edits), "Show DDL" from the metadata service's declared DDL kinds
/// (I8).
pub(crate) fn object_menu_for(
    hub: &ConnectionHub,
    connection: Option<ConnectionId>,
    path: &ObjectPath,
) -> Vec<MenuEntry> {
    let kind = path.kind();
    let allows = connection
        .and_then(|c| hub.sessions(c).first().map(TemplateAllowance::of_session))
        .unwrap_or_default();
    let ddl = match (connection, kind, hub.metadata()) {
        (Some(c), Some(k), Some(m)) => m.supports_ddl(c, k),
        _ => false,
    };
    object_menu_entries(kind, allows, ddl)
}

impl Navigator {
    /// The context menu's object entries of the selected row, for tests: what a right click on
    /// it shows before the generic entries ("Refresh navigator node"…).
    #[cfg(test)]
    pub(crate) fn selected_object_menu(&self, cx: &App) -> Vec<MenuEntry> {
        let Some(row) = self.selected_ix.and_then(|i| self.tree.rows().get(i)) else {
            return Vec::new();
        };
        match self.tree.object(row) {
            Some(o) => object_menu_for(self.hub.read(cx), MetaTree::connection_of(row), &o.path),
            None => Vec::new(),
        }
    }
}

/// The object part of a navigator row's context menu, in order. Table and view rows start with
/// "Open table data" (the action's own condition: a table-like object; it only reads, so a
/// read-only session offers it too), then "Open object editor" and the "Generate SQL" templates
/// the kind and the session `allows` (I8, review L2); "Show DDL" / "Copy DDL" only when the driver declares
/// DDL for the kind.
pub(crate) fn object_menu_entries(
    kind: Option<ninesql_core::driver_api::ObjectKind>,
    allows: TemplateAllowance,
    ddl: bool,
) -> Vec<MenuEntry> {
    let mut entries: Vec<MenuEntry> = Vec::new();
    if kind.is_some_and(is_table_like) {
        entries.push(Some((
            "Open table data".to_owned(),
            Box::new(OpenTableData),
        )));
        entries.push(Some((
            "Open object editor".to_owned(),
            Box::new(OpenObjectEditor),
        )));
        entries.push(None);
        for (t, action) in [
            (
                Template::Select,
                Box::new(GenerateSelectSql) as Box<dyn Action>,
            ),
            (Template::Insert, Box::new(GenerateInsertSql)),
            (Template::Update, Box::new(GenerateUpdateSql)),
            (Template::Delete, Box::new(GenerateDeleteSql)),
        ] {
            if kind.is_some_and(|k| t.offered(k, allows)) {
                entries.push(Some((format!("Generate SQL: {}", t.label()), action)));
            }
        }
    }
    if ddl {
        entries.push(Some(("Show DDL".to_owned(), Box::new(ShowObjectDdl))));
        entries.push(Some(("Copy DDL".to_owned(), Box::new(CopyObjectDdl))));
    }
    entries.push(None);
    entries
}

impl Focusable for Navigator {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Navigator {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let empty = self.tree.rows().is_empty();
        let count = self.tree.rows().len();
        v_flex()
            .id("navigator")
            .key_context(NAVIGATOR_CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::on_up))
            .on_action(cx.listener(Self::on_down))
            .on_action(cx.listener(Self::on_left))
            .on_action(cx.listener(Self::on_right))
            .on_action(cx.listener(Self::on_first))
            .on_action(cx.listener(Self::on_last))
            .on_action(cx.listener(Self::on_page_up))
            .on_action(cx.listener(Self::on_page_down))
            .on_action(cx.listener(Self::on_confirm))
            .size_full()
            .bg(theme.sidebar)
            .text_color(theme.sidebar_foreground)
            .text_sm()
            .child(
                div()
                    .px_3()
                    .py_2()
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.muted_foreground)
                    .child("DATABASE NAVIGATOR"),
            )
            .child(if empty {
                div()
                    .id("navigator-empty")
                    .px_3()
                    .py_2()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(no_connections_text(cx.try_global::<EffectiveKeymap>()))
                    .into_any_element()
            } else {
                uniform_list(
                    "navigator-rows",
                    count,
                    cx.processor(|this, range: Range<usize>, _window, cx| {
                        range
                            .filter_map(|ix| {
                                let row = this.tree.rows().get(ix)?.clone();
                                Some(this.render_row(ix, &row, cx))
                            })
                            .collect()
                    }),
                )
                .track_scroll(&self.scroll)
                .flex_1()
                .size_full()
                .into_any_element()
            })
    }
}
