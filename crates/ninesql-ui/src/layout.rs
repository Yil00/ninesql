//! Tabs and splits of the editor area, as a plain data model (F-WS-01).
//!
//! The editor area is a tree: leaves are panes (a tab strip with one active tab), inner nodes
//! are splits along one axis. The model holds only ids; the workspace view maps tab ids to
//! editor entities. Keeping it free of GPUI types makes every rule testable without a window.
//!
//! Rules:
//! - There is always at least one pane. The last pane is never removed, it can be empty.
//! - Closing the last tab of a pane removes the pane (unless it is the only one) and the split
//!   collapses when one child is left.
//! - Splitting puts a new pane right after the active pane. A split along the same axis as the
//!   parent adds a sibling; along the other axis it nests a new split.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Identifier of an editor tab, unique within one workspace window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TabId(u64);

/// Identifier of a pane (one tab strip), unique within one workspace window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PaneId(u64);

impl TabId {
    /// Raw value, for element ids.
    #[must_use]
    pub fn get(self) -> u64 {
        self.0
    }
}

impl PaneId {
    /// Raw value, for element ids.
    #[must_use]
    pub fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for PaneId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "pane-{}", self.0)
    }
}

/// Direction of a split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitAxis {
    /// Panes side by side ("split right").
    Horizontal,
    /// Panes stacked ("split down").
    Vertical,
}

/// One tab strip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pane {
    id: PaneId,
    tabs: Vec<TabId>,
    active: usize,
}

impl Pane {
    fn new(id: PaneId) -> Self {
        Self {
            id,
            tabs: Vec::new(),
            active: 0,
        }
    }

    /// Pane id.
    #[must_use]
    pub fn id(&self) -> PaneId {
        self.id
    }

    /// Tabs in display order.
    #[must_use]
    pub fn tabs(&self) -> &[TabId] {
        &self.tabs
    }

    /// Index of the active tab, `None` for an empty pane.
    #[must_use]
    pub fn active_index(&self) -> Option<usize> {
        (!self.tabs.is_empty()).then_some(self.active)
    }

    /// The active tab, `None` for an empty pane.
    #[must_use]
    pub fn active_tab(&self) -> Option<TabId> {
        self.tabs.get(self.active).copied()
    }
}

/// A node of the layout tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    /// A pane, by id.
    Pane(PaneId),
    /// Children laid out along `axis`; always two or more.
    Split {
        /// Direction.
        axis: SplitAxis,
        /// Children in display order.
        children: Vec<Node>,
    },
}

impl Node {
    fn collect_panes(&self, out: &mut Vec<PaneId>) {
        match self {
            Self::Pane(id) => out.push(*id),
            Self::Split { children, .. } => children.iter().for_each(|c| c.collect_panes(out)),
        }
    }

    /// Replaces the leaf `target` by `f(leaf)`. Returns whether it was found.
    fn replace_leaf(&mut self, target: PaneId, f: &mut dyn FnMut(Node) -> Node) -> bool {
        match self {
            Self::Pane(id) if *id == target => {
                let leaf = std::mem::replace(self, Node::Pane(target));
                *self = f(leaf);
                true
            }
            Self::Pane(_) => false,
            Self::Split { children, .. } => children.iter_mut().any(|c| c.replace_leaf(target, f)),
        }
    }

    /// Inserts `new` right after the leaf `target` when the parent split has `axis`.
    /// Returns whether it was inserted.
    fn insert_sibling(&mut self, target: PaneId, axis: SplitAxis, new: PaneId) -> bool {
        let Self::Split {
            axis: own,
            children,
        } = self
        else {
            return false;
        };
        if *own == axis
            && let Some(ix) = children
                .iter()
                .position(|c| matches!(c, Node::Pane(id) if *id == target))
        {
            children.insert(ix + 1, Node::Pane(new));
            return true;
        }
        children
            .iter_mut()
            .any(|c| c.insert_sibling(target, axis, new))
    }

    /// Removes the leaf `target`; collapses splits left with one child.
    fn remove_leaf(&mut self, target: PaneId) -> bool {
        let Self::Split { children, .. } = self else {
            return false;
        };
        let removed = if let Some(ix) = children
            .iter()
            .position(|c| matches!(c, Node::Pane(id) if *id == target))
        {
            children.remove(ix);
            true
        } else {
            children.iter_mut().any(|c| c.remove_leaf(target))
        };
        if removed
            && children.len() == 1
            && let Some(only) = children.pop()
        {
            *self = only;
        }
        removed
    }
}

/// The editor area: a layout tree, its panes and the active pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorLayout {
    root: Node,
    panes: Vec<Pane>,
    active_pane: PaneId,
    next_id: u64,
}

impl Default for EditorLayout {
    fn default() -> Self {
        Self::new()
    }
}

impl EditorLayout {
    /// One empty pane.
    #[must_use]
    pub fn new() -> Self {
        let first = PaneId(1);
        Self {
            root: Node::Pane(first),
            panes: vec![Pane::new(first)],
            active_pane: first,
            next_id: 2,
        }
    }

    fn next(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Allocates a tab id. The tab is not placed until [`Self::open_tab`].
    pub fn allocate_tab(&mut self) -> TabId {
        TabId(self.next())
    }

    /// The layout tree.
    #[must_use]
    pub fn root(&self) -> &Node {
        &self.root
    }

    /// A pane by id.
    #[must_use]
    pub fn pane(&self, id: PaneId) -> Option<&Pane> {
        self.panes.iter().find(|p| p.id == id)
    }

    fn pane_mut(&mut self, id: PaneId) -> Option<&mut Pane> {
        self.panes.iter_mut().find(|p| p.id == id)
    }

    /// Panes in layout order (depth first, left to right / top to bottom).
    #[must_use]
    pub fn pane_order(&self) -> Vec<PaneId> {
        let mut out = Vec::with_capacity(self.panes.len());
        self.root.collect_panes(&mut out);
        out
    }

    /// Number of panes.
    #[must_use]
    pub fn pane_count(&self) -> usize {
        self.panes.len()
    }

    /// The active pane id.
    #[must_use]
    pub fn active_pane_id(&self) -> PaneId {
        self.active_pane
    }

    /// The active pane.
    #[must_use]
    pub fn active_pane(&self) -> &Pane {
        // The active pane always exists: every removal re-targets it.
        self.pane(self.active_pane).unwrap_or(&self.panes[0])
    }

    /// The active tab of the active pane.
    #[must_use]
    pub fn active_tab(&self) -> Option<TabId> {
        self.active_pane().active_tab()
    }

    /// All tabs, in pane order.
    #[must_use]
    pub fn all_tabs(&self) -> Vec<TabId> {
        self.pane_order()
            .into_iter()
            .filter_map(|id| self.pane(id))
            .flat_map(|p| p.tabs.iter().copied())
            .collect()
    }

    /// The pane holding `tab`.
    #[must_use]
    pub fn pane_of(&self, tab: TabId) -> Option<PaneId> {
        self.panes
            .iter()
            .find(|p| p.tabs.contains(&tab))
            .map(|p| p.id)
    }

    /// Opens `tab` after the active tab of the active pane and activates it.
    pub fn open_tab(&mut self, tab: TabId) {
        let active = self.active_pane;
        if let Some(pane) = self.pane_mut(active) {
            let at = if pane.tabs.is_empty() {
                0
            } else {
                pane.active + 1
            };
            pane.tabs.insert(at, tab);
            pane.active = at;
        }
    }

    /// Makes `pane` the active pane. Returns false for an unknown pane.
    pub fn activate_pane(&mut self, pane: PaneId) -> bool {
        if self.pane(pane).is_some() {
            self.active_pane = pane;
            true
        } else {
            false
        }
    }

    /// Activates `tab` and its pane. Returns false for an unknown tab.
    pub fn activate_tab(&mut self, tab: TabId) -> bool {
        let Some(pane_id) = self.pane_of(tab) else {
            return false;
        };
        self.active_pane = pane_id;
        if let Some(pane) = self.pane_mut(pane_id)
            && let Some(ix) = pane.tabs.iter().position(|t| *t == tab)
        {
            pane.active = ix;
        }
        true
    }

    /// Next (`forward`) or previous tab of the active pane, wrapping around.
    pub fn cycle_tab(&mut self, forward: bool) -> Option<TabId> {
        let active = self.active_pane;
        let pane = self.pane_mut(active)?;
        let n = pane.tabs.len();
        if n == 0 {
            return None;
        }
        pane.active = if forward {
            (pane.active + 1) % n
        } else {
            (pane.active + n - 1) % n
        };
        pane.tabs.get(pane.active).copied()
    }

    /// Next (`forward`) or previous pane in layout order, wrapping around.
    pub fn cycle_pane(&mut self, forward: bool) -> PaneId {
        let order = self.pane_order();
        let n = order.len();
        if let Some(ix) = order.iter().position(|p| *p == self.active_pane) {
            let next = if forward {
                (ix + 1) % n
            } else {
                (ix + n - 1) % n
            };
            self.active_pane = order[next];
        }
        self.active_pane
    }

    /// Closes `tab`. When its pane becomes empty and is not the only pane, the pane is removed
    /// and the previous pane in layout order becomes active. Returns false for an unknown tab.
    pub fn close_tab(&mut self, tab: TabId) -> bool {
        let Some(pane_id) = self.pane_of(tab) else {
            return false;
        };
        let now_empty = match self.pane_mut(pane_id) {
            Some(pane) => {
                if let Some(ix) = pane.tabs.iter().position(|t| *t == tab) {
                    pane.tabs.remove(ix);
                    if pane.active > ix || pane.active >= pane.tabs.len() {
                        pane.active = pane.active.saturating_sub(1);
                    }
                }
                pane.tabs.is_empty()
            }
            None => return false,
        };
        if now_empty && self.panes.len() > 1 {
            self.remove_pane(pane_id);
        }
        true
    }

    /// Closes the active tab. Returns the closed tab.
    pub fn close_active_tab(&mut self) -> Option<TabId> {
        let tab = self.active_tab()?;
        self.close_tab(tab);
        Some(tab)
    }

    fn remove_pane(&mut self, pane: PaneId) {
        let order = self.pane_order();
        let ix = order.iter().position(|p| *p == pane).unwrap_or(0);
        if !self.root.remove_leaf(pane) {
            return;
        }
        self.panes.retain(|p| p.id != pane);
        if self.active_pane == pane {
            let remaining = self.pane_order();
            self.active_pane = remaining[ix.saturating_sub(1).min(remaining.len() - 1)];
        }
    }

    /// Splits the active pane along `axis`: a new pane holding `tab` is placed right after it
    /// and becomes active. Returns the new pane.
    pub fn split(&mut self, axis: SplitAxis, tab: TabId) -> PaneId {
        let target = self.active_pane;
        let new = PaneId(self.next());
        if !self.root.insert_sibling(target, axis, new) {
            self.root.replace_leaf(target, &mut |leaf| Node::Split {
                axis,
                children: vec![leaf, Node::Pane(new)],
            });
        }
        let mut pane = Pane::new(new);
        pane.tabs.push(tab);
        self.panes.push(pane);
        self.active_pane = new;
        new
    }
}

/// Deepest split nesting restored from a session file.
pub const MAX_SNAPSHOT_DEPTH: usize = 16;

/// The direction of a split in a [`LayoutSnapshot`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotAxis {
    /// Side by side.
    Horizontal,
    /// Stacked.
    Vertical,
}

impl From<SplitAxis> for SnapshotAxis {
    fn from(axis: SplitAxis) -> Self {
        match axis {
            SplitAxis::Horizontal => Self::Horizontal,
            SplitAxis::Vertical => Self::Vertical,
        }
    }
}

impl From<SnapshotAxis> for SplitAxis {
    fn from(axis: SnapshotAxis) -> Self {
        match axis {
            SnapshotAxis::Horizontal => Self::Horizontal,
            SnapshotAxis::Vertical => Self::Vertical,
        }
    }
}

/// The layout in the session file (F-WS-06): the split tree, each pane's tabs as indices into
/// the session's tab list, its active tab and which pane had the focus. Tabs that are not
/// restored (unsaved scripts, object editors) are left out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LayoutSnapshot {
    /// A tab strip.
    Pane {
        /// Indices into the session's tabs, in order.
        tabs: Vec<usize>,
        /// Position of the active tab in `tabs`.
        #[serde(default)]
        active: usize,
        /// The active pane of the window.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        focused: bool,
    },
    /// A split.
    Split {
        /// Direction.
        axis: SnapshotAxis,
        /// Children in order.
        children: Vec<LayoutSnapshot>,
    },
}

impl EditorLayout {
    /// The layout as a snapshot; `index` maps a tab to its index in the session's tab list,
    /// `None` for a tab that is not restored. Panes left with no tab are dropped (an empty
    /// layout is one empty pane).
    #[must_use]
    pub fn snapshot(&self, index: impl Fn(TabId) -> Option<usize>) -> LayoutSnapshot {
        self.snapshot_node(&self.root, &index)
            .unwrap_or(LayoutSnapshot::Pane {
                tabs: Vec::new(),
                active: 0,
                focused: true,
            })
    }

    fn snapshot_node(
        &self,
        node: &Node,
        index: &impl Fn(TabId) -> Option<usize>,
    ) -> Option<LayoutSnapshot> {
        match node {
            Node::Pane(id) => {
                let pane = self.pane(*id)?;
                let mut tabs = Vec::new();
                let mut active = 0;
                for (ix, tab) in pane.tabs.iter().enumerate() {
                    if let Some(i) = index(*tab) {
                        if ix <= pane.active {
                            active = tabs.len();
                        }
                        tabs.push(i);
                    }
                }
                (!tabs.is_empty()).then_some(LayoutSnapshot::Pane {
                    tabs,
                    active,
                    focused: *id == self.active_pane,
                })
            }
            Node::Split { axis, children } => {
                let mut kept: Vec<LayoutSnapshot> = children
                    .iter()
                    .filter_map(|c| self.snapshot_node(c, index))
                    .collect();
                match kept.len() {
                    0 => None,
                    1 => kept.pop(),
                    _ => Some(LayoutSnapshot::Split {
                        axis: (*axis).into(),
                        children: kept,
                    }),
                }
            }
        }
    }

    /// Replaces the panes and splits with `snapshot`, its tab indices mapped by `tab_for`
    /// (`None`: the tab was not restored). A tab placed twice is kept once; panes left with no
    /// tab are dropped and splits left with one child collapse; nesting deeper than
    /// [`MAX_SNAPSHOT_DEPTH`] is dropped. Returns the tabs placed.
    pub fn restore(
        &mut self,
        snapshot: &LayoutSnapshot,
        tab_for: impl Fn(usize) -> Option<TabId>,
    ) -> Vec<TabId> {
        let mut panes = Vec::new();
        let mut placed = Vec::new();
        let mut focused = None;
        let root = self.restore_node(snapshot, 0, &tab_for, &mut panes, &mut placed, &mut focused);
        match root {
            Some(root) => {
                self.root = root;
                let first = panes.first().map(|p: &Pane| p.id);
                self.panes = panes;
                self.active_pane = focused.or(first).unwrap_or(self.active_pane);
            }
            None => {
                let id = PaneId(self.next());
                self.root = Node::Pane(id);
                self.panes = vec![Pane::new(id)];
                self.active_pane = id;
            }
        }
        placed
    }

    fn restore_node(
        &mut self,
        node: &LayoutSnapshot,
        depth: usize,
        tab_for: &impl Fn(usize) -> Option<TabId>,
        panes: &mut Vec<Pane>,
        placed: &mut Vec<TabId>,
        focused: &mut Option<PaneId>,
    ) -> Option<Node> {
        if depth > MAX_SNAPSHOT_DEPTH {
            return None;
        }
        match node {
            LayoutSnapshot::Pane {
                tabs,
                active,
                focused: has_focus,
            } => {
                let mut pane = Pane::new(PaneId(0));
                for (ix, i) in tabs.iter().enumerate() {
                    if let Some(tab) = tab_for(*i).filter(|t| !placed.contains(t)) {
                        if ix <= *active {
                            pane.active = pane.tabs.len();
                        }
                        pane.tabs.push(tab);
                        placed.push(tab);
                    }
                }
                if pane.tabs.is_empty() {
                    return None;
                }
                pane.id = PaneId(self.next());
                if *has_focus {
                    *focused = Some(pane.id);
                }
                let id = pane.id;
                panes.push(pane);
                Some(Node::Pane(id))
            }
            LayoutSnapshot::Split { axis, children } => {
                let mut kept = Vec::new();
                for child in children {
                    if let Some(n) =
                        self.restore_node(child, depth + 1, tab_for, panes, placed, focused)
                    {
                        kept.push(n);
                    }
                }
                match kept.len() {
                    0 => None,
                    1 => kept.pop(),
                    _ => Some(Node::Split {
                        axis: (*axis).into(),
                        children: kept,
                    }),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{EditorLayout, LayoutSnapshot, Node, SplitAxis};

    fn layout_with_tabs(n: usize) -> EditorLayout {
        let mut layout = EditorLayout::new();
        for _ in 0..n {
            let tab = layout.allocate_tab();
            layout.open_tab(tab);
        }
        layout
    }

    #[test]
    fn starts_with_one_empty_pane() {
        let layout = EditorLayout::new();
        assert_eq!(layout.pane_count(), 1);
        assert_eq!(layout.active_tab(), None);
        assert_eq!(layout.active_pane().active_index(), None);
    }

    #[test]
    fn open_inserts_after_active_and_activates() {
        let mut layout = layout_with_tabs(2);
        let tabs = layout.all_tabs();
        assert_eq!(layout.active_tab(), Some(tabs[1]));
        layout.activate_tab(tabs[0]);
        let third = layout.allocate_tab();
        layout.open_tab(third);
        assert_eq!(layout.all_tabs(), vec![tabs[0], third, tabs[1]]);
        assert_eq!(layout.active_tab(), Some(third));
    }

    #[test]
    fn cycle_tabs_wraps() {
        let mut layout = layout_with_tabs(3);
        let tabs = layout.all_tabs();
        assert_eq!(layout.cycle_tab(true), Some(tabs[0]));
        assert_eq!(layout.cycle_tab(false), Some(tabs[2]));
        assert_eq!(layout.cycle_tab(false), Some(tabs[1]));
        let mut empty = EditorLayout::new();
        assert_eq!(empty.cycle_tab(true), None);
    }

    #[test]
    fn close_keeps_a_sensible_active_tab() {
        let mut layout = layout_with_tabs(3);
        let tabs = layout.all_tabs();
        layout.activate_tab(tabs[1]);
        // The tab to the right takes over, as in most editors.
        assert_eq!(layout.close_active_tab(), Some(tabs[1]));
        assert_eq!(layout.active_tab(), Some(tabs[2]));
        // Closing a tab before the active one keeps the same tab active.
        layout.close_tab(tabs[0]);
        assert_eq!(layout.active_tab(), Some(tabs[2]));
        // The last tab: its left neighbour takes over.
        let d = layout.allocate_tab();
        layout.open_tab(d);
        layout.close_tab(d);
        assert_eq!(layout.active_tab(), Some(tabs[2]));
        layout.close_tab(tabs[2]);
        assert_eq!(layout.active_tab(), None);
        assert_eq!(layout.pane_count(), 1, "the last pane is never removed");
        assert!(!layout.close_tab(tabs[2]), "unknown tab");
    }

    #[test]
    fn split_right_then_down_nests() {
        let mut layout = layout_with_tabs(1);
        let first_pane = layout.active_pane_id();
        let t2 = layout.allocate_tab();
        let right = layout.split(SplitAxis::Horizontal, t2);
        assert_eq!(layout.active_pane_id(), right);
        assert_eq!(layout.active_tab(), Some(t2));
        assert_eq!(
            layout.root(),
            &Node::Split {
                axis: SplitAxis::Horizontal,
                children: vec![Node::Pane(first_pane), Node::Pane(right)],
            }
        );
        // Same axis again: a sibling, not a nested split.
        layout.activate_pane(first_pane);
        let t3 = layout.allocate_tab();
        let middle = layout.split(SplitAxis::Horizontal, t3);
        assert_eq!(layout.pane_order(), vec![first_pane, middle, right]);
        // Other axis: nested.
        let t4 = layout.allocate_tab();
        let below = layout.split(SplitAxis::Vertical, t4);
        assert_eq!(
            layout.root(),
            &Node::Split {
                axis: SplitAxis::Horizontal,
                children: vec![
                    Node::Pane(first_pane),
                    Node::Split {
                        axis: SplitAxis::Vertical,
                        children: vec![Node::Pane(middle), Node::Pane(below)],
                    },
                    Node::Pane(right),
                ],
            }
        );
        assert_eq!(layout.pane_count(), 4);
    }

    #[test]
    fn closing_the_last_tab_of_a_pane_collapses_the_split() {
        let mut layout = layout_with_tabs(1);
        let first_pane = layout.active_pane_id();
        let t2 = layout.allocate_tab();
        layout.split(SplitAxis::Horizontal, t2);
        let t3 = layout.allocate_tab();
        let below = layout.split(SplitAxis::Vertical, t3);
        assert_eq!(layout.pane_count(), 3);
        layout.close_tab(t3);
        assert!(layout.pane(below).is_none());
        assert_eq!(layout.pane_count(), 2);
        // The nested split collapsed back to a plain sibling.
        assert!(matches!(
            layout.root(),
            Node::Split { axis: SplitAxis::Horizontal, children } if children.len() == 2
        ));
        layout.close_tab(t2);
        assert_eq!(layout.root(), &Node::Pane(first_pane));
        assert_eq!(layout.active_pane_id(), first_pane);
    }

    #[test]
    fn cycle_panes_in_layout_order() {
        let mut layout = layout_with_tabs(1);
        let p1 = layout.active_pane_id();
        let t = layout.allocate_tab();
        let p2 = layout.split(SplitAxis::Horizontal, t);
        let t = layout.allocate_tab();
        let p3 = layout.split(SplitAxis::Vertical, t);
        assert_eq!(layout.pane_order(), vec![p1, p2, p3]);
        assert_eq!(layout.cycle_pane(true), p1);
        assert_eq!(layout.cycle_pane(true), p2);
        assert_eq!(layout.cycle_pane(false), p1);
        assert_eq!(layout.cycle_pane(false), p3);
    }

    #[test]
    fn activate_tab_moves_the_active_pane() {
        let mut layout = layout_with_tabs(1);
        let first = layout.all_tabs()[0];
        let p1 = layout.active_pane_id();
        let t = layout.allocate_tab();
        layout.split(SplitAxis::Horizontal, t);
        assert!(layout.activate_tab(first));
        assert_eq!(layout.active_pane_id(), p1);
        assert_eq!(layout.pane_of(t), layout.pane_order().get(1).copied());
    }

    #[test]
    fn snapshot_and_restore_keep_splits_and_drop_missing_tabs() {
        let mut layout = layout_with_tabs(2);
        let tabs = layout.all_tabs();
        let t3 = layout.allocate_tab();
        layout.split(SplitAxis::Horizontal, t3);
        let t4 = layout.allocate_tab();
        layout.split(SplitAxis::Vertical, t4);
        // t2 is not restorable (an unsaved tab).
        let restorable = [tabs[0], t3, t4];
        let index = |t| restorable.iter().position(|r| *r == t);
        let snap = layout.snapshot(index);
        let json = serde_json::to_string(&snap).unwrap_or_default();
        let back: LayoutSnapshot = serde_json::from_str(&json).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(back, snap);

        // Restore in a fresh layout with t4's script gone.
        let mut fresh = EditorLayout::new();
        let new_tabs: Vec<_> = (0..3).map(|_| fresh.allocate_tab()).collect();
        let placed = fresh.restore(&snap, |i| (i != 2).then(|| new_tabs[i]));
        assert_eq!(placed, vec![new_tabs[0], new_tabs[1]]);
        assert_eq!(fresh.pane_count(), 2);
        assert!(matches!(
            fresh.root(),
            Node::Split { axis: SplitAxis::Horizontal, children } if children.len() == 2
        ));
        // The focused pane was the one of t4, gone: the first pane is active.
        assert_eq!(fresh.active_tab(), Some(new_tabs[0]));
        // Nothing restorable: one empty pane.
        let mut empty = EditorLayout::new();
        assert!(empty.restore(&snap, |_| None).is_empty());
        assert_eq!(empty.pane_count(), 1);
        assert!(empty.active_tab().is_none());
    }

    #[test]
    fn restore_ignores_duplicates_and_deep_nesting() {
        let mut snap = LayoutSnapshot::Pane {
            tabs: vec![0, 0, 1],
            active: 2,
            focused: true,
        };
        for _ in 0..40 {
            snap = LayoutSnapshot::Split {
                axis: super::SnapshotAxis::Vertical,
                children: vec![snap],
            };
        }
        let mut layout = EditorLayout::new();
        let t: Vec<_> = (0..2).map(|_| layout.allocate_tab()).collect();
        assert!(layout.restore(&snap, |i| t.get(i).copied()).is_empty());
        let flat = LayoutSnapshot::Pane {
            tabs: vec![0, 0, 1],
            active: 2,
            focused: true,
        };
        assert_eq!(layout.restore(&flat, |i| t.get(i).copied()), t);
        assert_eq!(layout.active_tab(), Some(t[1]));
    }
}
