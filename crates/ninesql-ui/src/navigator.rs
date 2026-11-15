//! Database navigator dock: the saved connections in their folders (F-CON-02), with the type
//! color, the type and read-only badges, a security badge and an "open" marker (ADR-0007 §4).
//! Metadata browsing arrives in P4.
//!
//! Keyboard: the tree handles up / down / left / right; the connection actions (connect, edit,
//! duplicate, delete…) apply to the selected connection through their key bindings and the
//! command palette.

use std::collections::BTreeMap;

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::list::ListItem;
use gpui_kit::component::tree::{TreeItem, TreeState, tree};
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use ninesql_core::ConnectionId;

use crate::connections::hub::{ConnectionHub, hub_or_install_empty};
use crate::connections::visuals::{ConnectionBadges, badges};

const CONNECTION_PREFIX: &str = "connection:";
const FOLDER_PREFIX: &str = "folder:";

/// The navigator view.
pub struct Navigator {
    tree: Entity<TreeState>,
    hub: Entity<ConnectionHub>,
    focus: FocusHandle,
    _observe: Subscription,
}

#[derive(Default)]
struct FolderNode {
    folders: BTreeMap<String, FolderNode>,
    connections: Vec<(String, ConnectionId)>,
}

impl FolderNode {
    fn into_items(self, path: &str) -> Vec<TreeItem> {
        let mut items: Vec<TreeItem> = self
            .folders
            .into_iter()
            .map(|(name, node)| {
                let child_path = if path.is_empty() {
                    name.clone()
                } else {
                    format!("{path}/{name}")
                };
                TreeItem::new(format!("{FOLDER_PREFIX}{child_path}"), name)
                    .expanded(true)
                    .children(node.into_items(&child_path))
            })
            .collect();
        let mut connections = self.connections;
        connections.sort_by_key(|(name, _)| name.to_lowercase());
        items.extend(
            connections
                .into_iter()
                .map(|(name, id)| TreeItem::new(format!("{CONNECTION_PREFIX}{id}"), name)),
        );
        items
    }
}

fn connection_id(item_id: &str) -> Option<ConnectionId> {
    let uuid = item_id.strip_prefix(CONNECTION_PREFIX)?;
    uuid.parse().ok().map(ConnectionId::from_uuid)
}

impl Navigator {
    /// The navigator of the application's connections.
    pub fn new(cx: &mut Context<Self>) -> Self {
        let hub = hub_or_install_empty(cx);
        let observe = cx.observe(&hub, |this: &mut Self, _, cx| this.rebuild(cx));
        let mut this = Self {
            tree: cx.new(|cx| TreeState::new(cx).items(Vec::new())),
            hub,
            focus: cx.focus_handle(),
            _observe: observe,
        };
        this.rebuild(cx);
        this
    }

    fn items(&self, cx: &App) -> Vec<TreeItem> {
        let mut root = FolderNode::default();
        for c in self.hub.read(cx).connections().iter() {
            let mut node = &mut root;
            for segment in &c.folder {
                node = node.folders.entry(segment.clone()).or_default();
            }
            node.connections.push((c.name.clone(), c.id));
        }
        root.into_items("")
    }

    /// Rebuilds the tree from the hub, keeping the selection.
    pub fn rebuild(&mut self, cx: &mut Context<Self>) {
        let selected = self.selected_connection(cx);
        let items = self.items(cx);
        self.tree.update(cx, |tree, cx| {
            tree.set_items(items, cx);
            if let Some(id) = selected {
                let ix = tree.index_of(&format!("{CONNECTION_PREFIX}{id}").into());
                tree.set_selected_index(ix, cx);
            }
        });
        cx.notify();
    }

    /// Whether there is no connection.
    #[must_use]
    pub fn is_empty(&self, cx: &App) -> bool {
        self.tree.read(cx).entry(0).is_none()
    }

    /// The selected connection, if a connection row is selected.
    #[must_use]
    pub fn selected_connection(&self, cx: &App) -> Option<ConnectionId> {
        self.tree
            .read(cx)
            .selected_item()
            .and_then(|item| connection_id(&item.id))
    }

    /// Selects a connection row.
    pub fn select_connection(&mut self, id: ConnectionId, cx: &mut Context<Self>) {
        let item_id: SharedString = format!("{CONNECTION_PREFIX}{id}").into();
        self.tree.update(cx, |tree, cx| {
            if let Some(ix) = tree.index_of(&item_id) {
                tree.set_selected_index(Some(ix), cx);
            }
        });
        cx.notify();
    }

    /// Row labels in display order with their depth, for tests: folders end with `/`.
    #[must_use]
    pub fn rows(&self, cx: &App) -> Vec<(usize, String)> {
        let tree = self.tree.read(cx);
        (0..)
            .map_while(|ix| tree.entry(ix))
            .map(|e| {
                let label = e.item().label.to_string();
                let label = if e.is_folder() {
                    format!("{label}/")
                } else {
                    label
                };
                (e.depth(), label)
            })
            .collect()
    }

    /// Moves keyboard focus to the tree.
    pub fn focus_tree(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_empty(cx) {
            self.focus.focus(window, cx);
        } else {
            self.tree.update(cx, |t, cx| t.focus(window, cx));
        }
    }
}

impl Focusable for Navigator {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Navigator {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let empty = self.is_empty(cx);
        let hub = self.hub.clone();
        v_flex()
            .id("navigator")
            .key_context("Navigator")
            .track_focus(&self.focus)
            .size_full()
            .bg(theme.sidebar)
            .text_color(theme.sidebar_foreground)
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
                    .child("No connections yet. Use \"New connection\" (Ctrl+Alt+N).")
                    .into_any_element()
            } else {
                div()
                    .flex_1()
                    .child(tree(&self.tree, move |ix, entry, selected, _window, cx| {
                        let label = entry.item().label.clone();
                        let row = ListItem::new(ix).selected(selected);
                        let Some(id) = connection_id(&entry.item().id) else {
                            return row
                                .child(div().text_color(cx.theme().muted_foreground).child(label));
                        };
                        let hub = hub.read(cx);
                        let Some(c) = hub.connection(id) else {
                            return row.child(label);
                        };
                        let sessions = hub.sessions(id);
                        let session_ro = sessions.first().map(|s| s.info().read_only);
                        let b = ConnectionBadges::of(c, hub.types(), session_ro);
                        let open = !sessions.is_empty();
                        let key = id.to_string();
                        row.child(
                            h_flex()
                                .gap_2()
                                .child(div().w(px(4.)).h(px(16.)).rounded_sm().bg(b.color))
                                .child(label)
                                .when(open, |el| {
                                    el.child(
                                        div()
                                            .id(SharedString::from(format!("open-{key}")))
                                            .text_xs()
                                            .text_color(cx.theme().success)
                                            .child("connected"),
                                    )
                                })
                                .when(!c.enabled, |el| {
                                    el.child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child("disabled"),
                                    )
                                })
                                .child(badges(&key, &b, false, cx)),
                        )
                    }))
                    .into_any_element()
            })
    }
}
