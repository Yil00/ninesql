//! The object editor (F-NAV-06): a tab for one object with Properties, Columns, Keys /
//! Constraints, Indexes and DDL tabs, as far as the metadata provides them.
//!
//! - Each list is loaded with `MetadataService::children` for the kinds the driver lists under
//!   the object (`child_kinds`, I8), when its tab is first shown; a kind the driver does not list
//!   shows "Not available for this engine" instead of an empty or invented list.
//! - Lists are virtualized (`uniform_list`): only visible rows are built (I6).
//! - The DDL tab appears only when the session declares DDL for the object's kind
//!   (`supports_ddl`, I8). It is read-only, highlighted with the editor's SQL grammar, and has a
//!   "Copy DDL" action.
//! - Everything is read through the metadata service; requests live in tasks dropped with the
//!   tab (closing the tab cancels its loads).

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Editor, EditorState};
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use ninesql_core::driver_api::ObjectKind;
use ninesql_core::driver_api::metadata::{ConstraintKind, ObjectDetails, ObjectInfo, ObjectPath};
use ninesql_core::{
    ConnectionId, InvalidationReason, InvalidationScope, MetadataEvent, MetadataService,
    NodeChildren, NodeKey, ObjectDdl,
};

use super::generate::{Template, TemplateAllowance};
use super::{display_name, display_path, kind_plural, kind_singular};
use crate::connections::hub::{ConnectionHub, MetadataUpdate};

/// "Not available" text of a list the driver does not provide (I8).
pub const NOT_AVAILABLE: &str = "Not available for this engine.";

/// The tabs of the object editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ObjectTab {
    /// Name, kind, path, comment and details.
    Properties,
    /// The columns.
    Columns,
    /// Constraints and foreign keys.
    Keys,
    /// The indexes.
    Indexes,
    /// The DDL (only when the driver declares it).
    Ddl,
}

impl ObjectTab {
    /// Its label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Properties => "Properties",
            Self::Columns => "Columns",
            Self::Keys => "Keys / Constraints",
            Self::Indexes => "Indexes",
            Self::Ddl => "DDL",
        }
    }

    /// The kinds listed in this tab.
    #[must_use]
    pub fn kinds(self) -> &'static [ObjectKind] {
        match self {
            Self::Columns => &[ObjectKind::Column],
            Self::Keys => &[ObjectKind::Constraint, ObjectKind::ForeignKey],
            Self::Indexes => &[ObjectKind::Index],
            Self::Properties | Self::Ddl => &[],
        }
    }
}

/// What the object editor asks of its workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObjectEditorEvent {
    /// Open a template in a new SQL editor.
    Generate(Template),
}

impl EventEmitter<ObjectEditorEvent> for ObjectEditor {}

/// The state of one loaded list.
#[derive(Debug, Clone, Default)]
pub struct Section {
    /// Loaded children.
    pub children: Option<Arc<NodeChildren>>,
    /// The last load failed.
    pub error: Option<String>,
    /// Loading.
    pub loading: bool,
}

/// The DDL state.
#[derive(Debug, Clone, Default)]
pub enum DdlState {
    /// Not asked yet.
    #[default]
    NotLoaded,
    /// Loading.
    Loading,
    /// Loaded.
    Loaded(Arc<ObjectDdl>),
    /// Failed (presentable).
    Failed(String),
}

/// What a load task is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum TaskKey {
    Info,
    List(ObjectKind),
    Ddl,
}

/// The object editor tab.
pub struct ObjectEditor {
    hub: Entity<ConnectionHub>,
    connection: ConnectionId,
    path: ObjectPath,
    title: SharedString,
    info: Option<ObjectInfo>,
    info_error: Option<String>,
    kinds: Vec<ObjectKind>,
    supports_ddl: bool,
    sections: HashMap<ObjectKind, Section>,
    ddl: DdlState,
    ddl_editor: Entity<EditorState>,
    active: ObjectTab,
    focus: FocusHandle,
    tasks: HashMap<TaskKey, Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl ObjectEditor {
    /// An editor for `path` of `connection`; `info` when the caller already has it.
    pub fn new(
        hub: Entity<ConnectionHub>,
        connection: ConnectionId,
        path: ObjectPath,
        info: Option<ObjectInfo>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let ddl_editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("sql")
                .line_number(true)
        });
        let subscriptions = vec![
            cx.subscribe(&hub, |this: &mut Self, _, event: &MetadataUpdate, cx| {
                this.on_metadata_event(&event.0, cx);
            }),
            cx.observe(&hub, |_, _, cx| cx.notify()),
        ];
        let name = path
            .leaf()
            .map(|s| display_name(&s.name))
            .unwrap_or_default();
        let mut this = Self {
            hub,
            connection,
            title: name.into(),
            path,
            info,
            info_error: None,
            kinds: Vec::new(),
            supports_ddl: false,
            sections: HashMap::new(),
            ddl: DdlState::NotLoaded,
            ddl_editor,
            active: ObjectTab::Properties,
            focus: cx.focus_handle(),
            tasks: HashMap::new(),
            _subscriptions: subscriptions,
        };
        this.read_capabilities(cx);
        this.load_info(cx);
        this
    }

    fn service(&self, cx: &App) -> Option<MetadataService> {
        self.hub.read(cx).metadata()
    }

    fn read_capabilities(&mut self, cx: &App) {
        let Some(service) = self.service(cx) else {
            return;
        };
        self.kinds = service
            .child_kinds(self.connection, &self.path)
            .unwrap_or_default();
        self.supports_ddl = self
            .path
            .kind()
            .is_some_and(|k| service.supports_ddl(self.connection, k));
    }

    /// Whether the open session of the connection is read-only (write templates are not
    /// offered then, I8).
    #[must_use]
    pub fn read_only(&self, cx: &App) -> bool {
        self.hub
            .read(cx)
            .sessions(self.connection)
            .first()
            .is_some_and(|s| s.info().read_only)
    }

    /// The templates the open session allows (read-only, declared row edits; review L2).
    #[must_use]
    pub fn template_allowance(&self, cx: &App) -> TemplateAllowance {
        self.hub
            .read(cx)
            .sessions(self.connection)
            .first()
            .map(TemplateAllowance::of_session)
            .unwrap_or_default()
    }

    /// The "Generate SQL" buttons of the toolbar, in order.
    #[must_use]
    pub fn offered_templates(&self, cx: &App) -> Vec<Template> {
        let kind = self.path.kind();
        let allows = self.template_allowance(cx);
        Template::ALL
            .into_iter()
            .filter(|t| kind.is_some_and(|k| t.offered(k, allows)))
            .collect()
    }

    /// Bidirectional controls in the loaded DDL (flagged like in the SQL editor).
    #[must_use]
    pub fn ddl_bidi_controls(&self) -> Vec<ninesql_editor::hygiene::BidiControl> {
        self.ddl_text()
            .map(ninesql_editor::hygiene::find_bidi_controls)
            .unwrap_or_default()
    }

    /// Tab title (the object's name).
    #[must_use]
    pub fn title(&self) -> &SharedString {
        &self.title
    }

    /// The connection.
    #[must_use]
    pub fn connection(&self) -> ConnectionId {
        self.connection
    }

    /// The object.
    #[must_use]
    pub fn path(&self) -> &ObjectPath {
        &self.path
    }

    /// The object's listing (comment, details), once known.
    #[must_use]
    pub fn info(&self) -> Option<&ObjectInfo> {
        self.info.as_ref()
    }

    /// The tabs shown: the DDL tab only when the driver declares DDL for the kind (I8).
    #[must_use]
    pub fn tabs(&self) -> Vec<ObjectTab> {
        let mut tabs = vec![
            ObjectTab::Properties,
            ObjectTab::Columns,
            ObjectTab::Keys,
            ObjectTab::Indexes,
        ];
        if self.supports_ddl {
            tabs.push(ObjectTab::Ddl);
        }
        tabs
    }

    /// Whether the driver lists anything for `tab` under this object.
    #[must_use]
    pub fn available(&self, tab: ObjectTab) -> bool {
        match tab {
            ObjectTab::Properties => true,
            ObjectTab::Ddl => self.supports_ddl,
            _ => tab.kinds().iter().any(|k| self.kinds.contains(k)),
        }
    }

    /// The active tab.
    #[must_use]
    pub fn active(&self) -> ObjectTab {
        self.active
    }

    /// A loaded list.
    #[must_use]
    pub fn section(&self, kind: ObjectKind) -> Option<&Section> {
        self.sections.get(&kind)
    }

    /// The DDL state.
    #[must_use]
    pub fn ddl(&self) -> &DdlState {
        &self.ddl
    }

    /// The DDL text, once loaded.
    #[must_use]
    pub fn ddl_text(&self) -> Option<&str> {
        match &self.ddl {
            DdlState::Loaded(d) => Some(d.text.as_str()),
            _ => None,
        }
    }

    /// Shows a tab, loading what it needs the first time.
    pub fn activate(&mut self, tab: ObjectTab, window: &mut Window, cx: &mut Context<Self>) {
        if !self.tabs().contains(&tab) {
            return;
        }
        self.active = tab;
        self.load_tab(tab, window, cx);
        cx.notify();
    }

    fn load_tab(&mut self, tab: ObjectTab, window: &mut Window, cx: &mut Context<Self>) {
        match tab {
            ObjectTab::Properties => {}
            ObjectTab::Ddl => self.load_ddl(window, cx),
            _ => {
                for kind in tab.kinds() {
                    if self.kinds.contains(kind) {
                        self.load_section(*kind, cx);
                    }
                }
                // Primary-key columns come from the column details.
                if tab == ObjectTab::Keys && self.kinds.contains(&ObjectKind::Column) {
                    self.load_section(ObjectKind::Column, cx);
                }
            }
        }
    }

    fn load_info(&mut self, cx: &mut Context<Self>) {
        if self.info.is_some() {
            return;
        }
        let Some(service) = self.service(cx) else {
            return;
        };
        let Some((leaf, parents)) = self.path.0.split_last() else {
            return;
        };
        let key = NodeKey::new(ObjectPath(parents.to_vec()), leaf.kind);
        let path = self.path.clone();
        if let Some(c) = service.cached_children(self.connection, &key) {
            self.info = c.items.iter().find(|i| i.path == path).cloned();
            return;
        }
        let request = service.children(self.connection, key);
        let task = cx.spawn(async move |this, cx| {
            let result = request.await;
            let _ = this.update(cx, |e, cx| {
                e.tasks.remove(&TaskKey::Info);
                match result {
                    Ok(c) => {
                        e.info = c.items.iter().find(|i| i.path == path).cloned();
                        if e.info.is_none() {
                            e.info_error = Some(
                                "The object is not listed any more (or a filter hides it).".into(),
                            );
                        }
                    }
                    Err(err) => e.info_error = Some(super::display_message(&err.to_string())),
                }
                cx.notify();
            });
        });
        self.tasks.insert(TaskKey::Info, task);
    }

    fn load_section(&mut self, kind: ObjectKind, cx: &mut Context<Self>) {
        let section = self.sections.entry(kind).or_default();
        if section.children.is_some() || section.loading {
            return;
        }
        let Some(service) = self.hub.read(cx).metadata() else {
            return;
        };
        let key = NodeKey::new(self.path.clone(), kind);
        if let Some(c) = service.cached_children(self.connection, &key) {
            section.children = Some(c);
            return;
        }
        section.loading = true;
        section.error = None;
        let request = service.children(self.connection, key);
        let task = cx.spawn(async move |this, cx| {
            let result = request.await;
            let _ = this.update(cx, |e, cx| {
                e.tasks.remove(&TaskKey::List(kind));
                let section = e.sections.entry(kind).or_default();
                section.loading = false;
                match result {
                    Ok(c) => section.children = Some(c),
                    Err(err) => section.error = Some(super::display_message(&err.to_string())),
                }
                cx.notify();
            });
        });
        self.tasks.insert(TaskKey::List(kind), task);
    }

    fn load_ddl(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.supports_ddl || matches!(self.ddl, DdlState::Loading | DdlState::Loaded(_)) {
            return;
        }
        let Some(service) = self.service(cx) else {
            return;
        };
        if let Some(d) = service.cached_ddl(self.connection, &self.path) {
            self.show_ddl(d, window, cx);
            return;
        }
        self.ddl = DdlState::Loading;
        let request = service.ddl(self.connection, self.path.clone());
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = request.await;
            let _ = this.update_in(cx, |e, window, cx| {
                match result {
                    Ok(d) => e.show_ddl(d, window, cx),
                    Err(err) => e.ddl = DdlState::Failed(super::display_message(&err.to_string())),
                }
                cx.notify();
            });
        });
        self.tasks.insert(TaskKey::Ddl, task);
    }

    fn show_ddl(&mut self, ddl: Arc<ObjectDdl>, window: &mut Window, cx: &mut Context<Self>) {
        let text = ddl.text.clone();
        self.ddl_editor
            .update(cx, |s, cx| s.set_value(text, window, cx));
        self.ddl = DdlState::Loaded(ddl);
    }

    /// Copies the DDL to the clipboard. Returns false when it is not loaded.
    pub fn copy_ddl(&self, cx: &mut App) -> bool {
        let Some(text) = self.ddl_text() else {
            return false;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(text.to_owned()));
        true
    }

    /// Reloads everything shown (Refresh).
    pub fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(service) = self.service(cx) {
            service.invalidate(self.connection, &self.path);
        }
        self.reset(window, cx);
    }

    fn reset(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.tasks.clear();
        self.sections.clear();
        self.ddl = DdlState::NotLoaded;
        self.read_capabilities(cx);
        self.load_info(cx);
        let tab = self.active;
        self.load_tab(tab, window, cx);
        cx.notify();
    }

    fn covers(&self, scope: &InvalidationScope) -> bool {
        let related = |p: &ObjectPath| is_under(&self.path, p) || is_under(p, &self.path);
        match scope {
            InvalidationScope::Subtree(p) => related(p),
            InvalidationScope::Node(k) => related(&k.parent),
            // `Connection`, and scopes added later.
            _ => true,
        }
    }

    fn on_metadata_event(&mut self, event: &MetadataEvent, cx: &mut Context<Self>) {
        let MetadataEvent::Invalidated {
            connection,
            scope,
            reason,
        } = event
        else {
            return;
        };
        if *connection != self.connection || !self.covers(scope) {
            return;
        }
        if *reason == InvalidationReason::SessionClosed {
            // What is shown stays readable; nothing more can load.
            self.tasks.clear();
            for section in self.sections.values_mut() {
                section.loading = false;
                if section.children.is_none() {
                    section.error = Some("The session is closed: reconnect, then Refresh.".into());
                }
            }
            if matches!(self.ddl, DdlState::Loading) {
                self.ddl =
                    DdlState::Failed("The session is closed: reconnect, then Refresh.".into());
            }
            cx.notify();
            return;
        }
        // The lists shown so far reload (the DDL text stays until Refresh).
        let kinds: Vec<ObjectKind> = self.sections.keys().copied().collect();
        for kind in kinds {
            self.tasks.remove(&TaskKey::List(kind));
            self.sections.insert(kind, Section::default());
            self.load_section(kind, cx);
        }
        cx.notify();
    }

    fn render_properties(&self, cx: &App) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let mut props: Vec<(String, String)> = Vec::new();
        let kind = self.path.kind();
        props.push(("Name".into(), self.title.to_string()));
        props.push((
            "Kind".into(),
            kind.map_or("object", kind_singular).to_owned(),
        ));
        if let Some((_, parents)) = self.path.0.split_last() {
            for seg in parents {
                let label = kind_singular(seg.kind);
                let mut label = label.to_owned();
                if let Some(first) = label.get_mut(..1) {
                    first.make_ascii_uppercase();
                }
                props.push((label, display_name(&seg.name)));
            }
        }
        if let Some(name) = self
            .hub
            .read(cx)
            .connection(self.connection)
            .map(|c| display_name(&c.name))
        {
            props.push(("Connection".into(), name));
        }
        if let Some(info) = &self.info {
            if let Some(c) = &info.comment {
                props.push(("Comment".into(), display_name(c)));
            }
            match &info.details {
                ObjectDetails::Table(t) => {
                    if let Some(rows) = t.estimated_rows {
                        props.push(("Estimated rows".into(), rows.to_string()));
                    }
                    if let Some(f) = &t.flavour {
                        props.push(("Flavour".into(), display_name(f)));
                    }
                }
                ObjectDetails::Column(c) => {
                    props.push(("Type".into(), display_name(&c.data_type)));
                    props.push(("Nullable".into(), yes_no(c.nullable).into()));
                }
                _ => {}
            }
        }
        v_flex()
            .gap_1()
            .p_3()
            .text_sm()
            .children(props.into_iter().map(|(k, v)| {
                h_flex()
                    .gap_3()
                    .child(div().w(px(140.)).text_color(muted).child(k))
                    .child(div().flex_1().child(v))
            }))
            .when_some(self.info_error.clone(), |el, e| {
                el.child(div().text_color(muted).child(e))
            })
            .into_any_element()
    }

    fn render_list(&self, tab: ObjectTab, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        if !self.available(tab) {
            return div()
                .p_3()
                .text_sm()
                .text_color(muted)
                .child(NOT_AVAILABLE)
                .into_any_element();
        }
        let mut blocks = Vec::new();
        if tab == ObjectTab::Keys {
            let pk: Vec<String> = self
                .sections
                .get(&ObjectKind::Column)
                .and_then(|s| s.children.as_ref())
                .map(|c| {
                    c.items
                        .iter()
                        .filter(|i| matches!(&i.details, ObjectDetails::Column(c) if c.primary_key))
                        .map(|i| display_name(i.name()))
                        .collect()
                })
                .unwrap_or_default();
            if !pk.is_empty() {
                blocks.push(
                    div()
                        .px_3()
                        .pt_2()
                        .text_sm()
                        .child(format!("Primary key columns: {}", pk.join(", ")))
                        .into_any_element(),
                );
            }
        }
        for kind in tab.kinds() {
            if !self.kinds.contains(kind) {
                continue;
            }
            blocks.push(self.render_section(*kind, cx));
        }
        v_flex().size_full().children(blocks).into_any_element()
    }

    fn render_section(&self, kind: ObjectKind, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let section = self.sections.get(&kind).cloned().unwrap_or_default();
        let header = div()
            .px_3()
            .pt_2()
            .text_xs()
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(muted)
            .child(kind_plural(kind).to_uppercase());
        let body: AnyElement = if let Some(e) = &section.error {
            div()
                .px_3()
                .text_sm()
                .text_color(theme.danger)
                .child(e.clone())
                .into_any_element()
        } else if let Some(children) = section.children.clone() {
            if children.items.is_empty() {
                div()
                    .px_3()
                    .text_sm()
                    .text_color(muted)
                    .child("None.")
                    .into_any_element()
            } else {
                let count = children.items.len();
                let header_row = columns_header(kind, muted);
                let more = (!children.complete).then(|| {
                    div()
                        .px_3()
                        .text_xs()
                        .text_color(muted)
                        .child("More are listed in the navigator (\"Load more\").")
                });
                v_flex()
                    .flex_1()
                    .min_h(px(120.))
                    .child(header_row)
                    .child(
                        uniform_list(
                            SharedString::from(format!("object-list-{kind:?}")),
                            count,
                            cx.processor(move |this, range: Range<usize>, _window, cx| {
                                let muted = cx.theme().muted_foreground;
                                let Some(children) =
                                    this.sections.get(&kind).and_then(|s| s.children.clone())
                                else {
                                    return Vec::new();
                                };
                                range
                                    .filter_map(|ix| {
                                        let item = children.items.get(ix)?;
                                        Some(list_row(kind, ix, item, muted))
                                    })
                                    .collect()
                            }),
                        )
                        .flex_1()
                        .min_h(px(96.)),
                    )
                    .children(more)
                    .into_any_element()
            }
        } else {
            div()
                .px_3()
                .text_sm()
                .text_color(muted)
                .child("Loading…")
                .into_any_element()
        };
        v_flex()
            .flex_1()
            .gap_1()
            .child(header)
            .child(body)
            .into_any_element()
    }

    fn render_ddl(&self, cx: &App) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        match &self.ddl {
            DdlState::Loaded(_) => v_flex()
                .size_full()
                .children(ddl_bidi_banner(&self.ddl_bidi_controls(), cx))
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .child(Editor::new(&self.ddl_editor).readonly(true).h_full()),
                )
                .into_any_element(),
            DdlState::Failed(e) => div()
                .p_3()
                .text_sm()
                .text_color(cx.theme().danger)
                .child(e.clone())
                .into_any_element(),
            DdlState::Loading | DdlState::NotLoaded => div()
                .p_3()
                .text_sm()
                .text_color(muted)
                .child("Loading…")
                .into_any_element(),
        }
    }
}

/// `true` when `path` is `prefix` or lies under it.
fn is_under(path: &ObjectPath, prefix: &ObjectPath) -> bool {
    path.0.len() >= prefix.0.len() && path.0[..prefix.0.len()] == prefix.0[..]
}

/// The warning shown over DDL with bidirectional controls, as in the SQL editor (P4-B review
/// L2).
#[must_use]
pub fn ddl_bidi_text(controls: &[ninesql_editor::hygiene::BidiControl]) -> Option<String> {
    let first = controls.first()?;
    Some(format!(
        "{} bidirectional control character(s) in this DDL (first: {}): the text may read differently from what it defines.",
        controls.len(),
        first.label()
    ))
}

fn ddl_bidi_banner(
    controls: &[ninesql_editor::hygiene::BidiControl],
    cx: &App,
) -> Option<AnyElement> {
    let text = ddl_bidi_text(controls)?;
    let theme = cx.theme();
    Some(
        div()
            .id("ddl-bidi")
            .px_2()
            .py_1()
            .bg(theme.warning)
            .text_color(theme.warning_foreground)
            .child(text)
            .into_any_element(),
    )
}

fn yes_no(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}

/// The "Details" cell of a constraint, foreign key or index (P4-A details): kind and columns,
/// referenced table and columns, uniqueness, method and predicate. Names are shown with
/// [`display_name`].
#[must_use]
pub fn detail_text(details: &ObjectDetails) -> String {
    let list = |cols: &[String]| {
        format!(
            "({})",
            cols.iter()
                .map(|c| display_name(c))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    match details {
        ObjectDetails::Constraint(c) => {
            let kind = match c.kind {
                ConstraintKind::PrimaryKey => "primary key",
                ConstraintKind::Unique => "unique",
                ConstraintKind::Check => "check",
                ConstraintKind::Exclusion => "exclusion",
                _ => "constraint",
            };
            let mut out = kind.to_owned();
            if !c.columns.is_empty() {
                out.push(' ');
                out.push_str(&list(&c.columns));
            }
            if c.kind == ConstraintKind::Check
                && let Some(d) = &c.definition
            {
                out.push_str(": ");
                out.push_str(&display_name(d));
            }
            out
        }
        ObjectDetails::ForeignKey(fk) => {
            let mut out = format!(
                "{} → {}",
                list(&fk.columns),
                display_path(&fk.referenced_table)
            );
            if !fk.referenced_columns.is_empty() {
                out.push(' ');
                out.push_str(&list(&fk.referenced_columns));
            }
            out
        }
        ObjectDetails::Index(ix) => {
            let mut parts = Vec::new();
            if ix.primary {
                parts.push("primary".to_owned());
            } else if ix.unique {
                parts.push("unique".to_owned());
            }
            parts.push(list(&ix.columns));
            if let Some(m) = &ix.method {
                parts.push(display_name(m));
            }
            // P5-B flags: a partial unique index must not look like a plain unique one.
            if ix.partial || ix.predicate.is_some() {
                parts.push("partial".to_owned());
            }
            if !ix.valid {
                parts.push("invalid".to_owned());
            }
            if ix.deferrable {
                parts.push("deferrable".to_owned());
            }
            if !ix.nulls_distinct {
                parts.push("NULLS NOT DISTINCT".to_owned());
            }
            if let Some(p) = &ix.predicate {
                parts.push(format!("WHERE {}", display_name(p)));
            }
            parts.join(" · ")
        }
        _ => String::new(),
    }
}

fn columns_header(kind: ObjectKind, muted: Hsla) -> AnyElement {
    let cells: &[(&str, f32)] = if kind == ObjectKind::Column {
        &[
            ("#", 36.),
            ("Name", 180.),
            ("Type", 160.),
            ("Null", 48.),
            ("Default", 140.),
            ("PK", 36.),
            ("Auto", 44.),
            ("Comment", 220.),
        ]
    } else {
        &[("Name", 220.), ("Details", 380.), ("Comment", 220.)]
    };
    h_flex()
        .px_3()
        .text_xs()
        .text_color(muted)
        .children(
            cells
                .iter()
                .map(|(t, w)| div().w(px(*w)).flex_none().child(*t)),
        )
        .into_any_element()
}

fn list_row(kind: ObjectKind, ix: usize, item: &ObjectInfo, muted: Hsla) -> AnyElement {
    let comment = item
        .comment
        .as_deref()
        .map(display_name)
        .unwrap_or_default();
    let cell = |w: f32, text: String| div().w(px(w)).flex_none().overflow_hidden().child(text);
    let row = h_flex().id(("object-row", ix)).h(px(22.)).px_3().text_sm();
    if kind == ObjectKind::Column {
        let (ordinal, ty, nullable, default, pk, auto) = match &item.details {
            ObjectDetails::Column(c) => (
                c.ordinal.to_string(),
                display_name(&c.data_type),
                yes_no(c.nullable).to_owned(),
                c.default.as_deref().map(display_name).unwrap_or_default(),
                if c.primary_key { "yes" } else { "" }.to_owned(),
                if c.auto_generated { "yes" } else { "" }.to_owned(),
            ),
            _ => Default::default(),
        };
        row.child(cell(36., ordinal).text_color(muted))
            .child(cell(180., display_name(item.name())))
            .child(cell(160., ty))
            .child(cell(48., nullable))
            .child(cell(140., default))
            .child(cell(36., pk))
            .child(cell(44., auto))
            .child(cell(220., comment).text_color(muted))
            .into_any_element()
    } else {
        row.child(cell(220., display_name(item.name())))
            .child(cell(380., detail_text(&item.details)))
            .child(cell(220., comment).text_color(muted))
            .into_any_element()
    }
}

impl Focusable for ObjectEditor {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for ObjectEditor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let tabs = self.tabs();
        let active_ix = tabs.iter().position(|t| *t == self.active).unwrap_or(0);
        let bar = TabBar::new("object-tabs")
            .children(tabs.iter().map(|t| Tab::new().label(t.label())))
            .selected_index(active_ix)
            .on_click(cx.listener({
                let tabs = tabs.clone();
                move |this, ix: &usize, window, cx| {
                    if let Some(tab) = tabs.get(*ix) {
                        this.activate(*tab, window, cx);
                    }
                }
            }));
        let kind = self.path.kind();
        let templates = self.offered_templates(cx);
        let toolbar = h_flex()
            .px_3()
            .py_1()
            .gap_2()
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(self.title.clone()),
            )
            .child(div().flex_1().text_xs().text_color(muted).child(format!(
                "{} · {}",
                kind.map_or("object", kind_singular),
                display_path(&self.path)
            )))
            .children(templates.into_iter().map(|t| {
                Button::new(SharedString::from(format!("object-gen-{}", t.label())))
                    .ghost()
                    .xsmall()
                    .label(t.label())
                    .tooltip(format!("Generate SQL: {}", t.label()))
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.emit(ObjectEditorEvent::Generate(t));
                    }))
            }))
            .when(
                self.active == ObjectTab::Ddl && self.ddl_text().is_some(),
                |el| {
                    el.child(
                        Button::new("object-copy-ddl")
                            .ghost()
                            .xsmall()
                            .label("Copy DDL")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.copy_ddl(cx);
                            })),
                    )
                },
            )
            .child(
                Button::new("object-refresh")
                    .ghost()
                    .xsmall()
                    .label("Refresh")
                    .on_click(cx.listener(|this, _, window, cx| this.refresh(window, cx))),
            );
        let body = match self.active {
            ObjectTab::Properties => self.render_properties(cx),
            ObjectTab::Ddl => self.render_ddl(cx),
            tab => self.render_list(tab, cx),
        };
        v_flex()
            .id("object-editor")
            .track_focus(&self.focus)
            .size_full()
            .child(toolbar)
            .child(bar)
            .child(div().flex_1().min_h_0().child(body))
    }
}

#[cfg(test)]
mod tests {
    use super::{ddl_bidi_text, detail_text};
    use ninesql_core::driver_api::ObjectKind;
    use ninesql_core::driver_api::metadata::{
        ConstraintInfo, ConstraintKind, ForeignKeyInfo, IndexInfo, ObjectDetails, ObjectPath,
    };

    #[test]
    fn details_show_keys_references_and_indexes() {
        let mut pk = ConstraintInfo::new(ConstraintKind::PrimaryKey);
        pk.columns = vec!["tenant_id".into(), "id".into()];
        assert_eq!(
            detail_text(&ObjectDetails::Constraint(pk)),
            "primary key (tenant_id, id)"
        );
        let target = ObjectPath::root()
            .child(ObjectKind::Schema, "public")
            .child(ObjectKind::Table, "customers");
        let mut fk = ForeignKeyInfo::new(target);
        fk.columns = vec!["customer_id".into()];
        fk.referenced_columns = vec!["id\u{202e}".into()];
        assert_eq!(
            detail_text(&ObjectDetails::ForeignKey(fk)),
            "(customer_id) → public.customers (id\u{FFFD})"
        );
        let mut ix = IndexInfo::default();
        ix.columns = vec!["lower(email)".into()];
        ix.unique = true;
        ix.method = Some("btree".into());
        ix.valid = true;
        assert_eq!(
            detail_text(&ObjectDetails::Index(ix)),
            "unique · (lower(email)) · btree"
        );
    }

    #[test]
    fn index_flags_are_shown() {
        // A SQLite partial unique index: no predicate text, but flagged partial.
        let mut ix = IndexInfo::default();
        ix.columns = vec!["email".into()];
        ix.unique = true;
        ix.partial = true;
        ix.valid = true;
        assert_eq!(
            detail_text(&ObjectDetails::Index(ix)),
            "unique · (email) · partial"
        );
        let mut ix = IndexInfo::default();
        ix.columns = vec!["a".into()];
        ix.unique = true;
        ix.valid = false;
        ix.deferrable = true;
        ix.nulls_distinct = false;
        ix.predicate = Some("a > 0".into());
        assert_eq!(
            detail_text(&ObjectDetails::Index(ix)),
            "unique · (a) · partial · invalid · deferrable · NULLS NOT DISTINCT · WHERE a > 0"
        );
    }

    #[test]
    fn ddl_with_bidi_controls_is_flagged() {
        let controls =
            ninesql_editor::hygiene::find_bidi_controls("CREATE TABLE \"a\u{202e}b\" ()");
        let text = ddl_bidi_text(&controls).unwrap_or_default();
        assert!(
            text.starts_with("1 bidirectional control character(s) in this DDL"),
            "{text}"
        );
        assert!(ddl_bidi_text(&[]).is_none());
    }
}
