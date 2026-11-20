//! "Find object" (F-NAV-05): a palette-like dialog over `MetadataService::search`, across one or
//! more open connections, optionally matching comments and loaded definitions (DDL). Search
//! reads the metadata cache only and never crawls the server; "Load metadata" starts a bounded
//! background prefetch (`MetadataService::prefetch`) of the chosen connections, cancelled when
//! the dialog closes.
//!
//! Results show the connection, the path and the kind. Enter (or a click) reveals the object in
//! the navigator, expanding the path lazily; Ctrl+Enter (or "Open") opens the object editor.

use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use ninesql_core::driver_api::metadata::ObjectPath;
use ninesql_core::{
    ConnectionId, MetadataService, Prefetch, PrefetchLimits, SearchHit, SearchRequest,
    SearchResults,
};

use super::{display_path, kind_singular};

/// Most results asked for.
const RESULT_LIMIT: usize = 200;

/// What the user chose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FindChoice {
    /// Reveal the object in the navigator.
    Reveal(ConnectionId, ObjectPath),
    /// Open the object editor.
    Open(ConnectionId, ObjectPath),
}

/// Called with the user's choice (the dialog is closed first).
pub type OnChoose = Rc<dyn Fn(FindChoice, &mut Window, &mut App)>;

/// One connection the search can cover.
#[derive(Debug, Clone)]
pub struct SearchScope {
    /// The connection.
    pub id: ConnectionId,
    /// Its name.
    pub name: String,
    /// Included in the search.
    pub selected: bool,
}

/// The dialog's content.
pub struct FindObjectDialog {
    service: MetadataService,
    query: Entity<InputState>,
    scopes: Vec<SearchScope>,
    in_comments: bool,
    in_definitions: bool,
    results: Option<SearchResults>,
    selected: usize,
    generation: u64,
    searching: Option<Task<()>>,
    prefetches: Vec<Prefetch>,
    on_choose: OnChoose,
    _subscriptions: Vec<Subscription>,
}

impl FindObjectDialog {
    /// A dialog over `scopes` (the open connections; the selected ones are searched).
    pub fn new(
        service: MetadataService,
        scopes: Vec<SearchScope>,
        on_choose: OnChoose,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let query = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Object name (fuzzy), e.g. ordit for order_items")
        });
        let subscriptions = vec![cx.subscribe_in(
            &query,
            window,
            |this: &mut Self, _, event: &InputEvent, window, cx| match event {
                InputEvent::Change => this.search(cx),
                InputEvent::PressEnter { secondary, .. } => this.choose(*secondary, window, cx),
                _ => {}
            },
        )];
        Self {
            service,
            query,
            scopes,
            in_comments: false,
            in_definitions: false,
            results: None,
            selected: 0,
            generation: 0,
            searching: None,
            prefetches: Vec::new(),
            on_choose,
            _subscriptions: subscriptions,
        }
    }

    /// The query input.
    #[must_use]
    pub fn query(&self) -> &Entity<InputState> {
        &self.query
    }

    /// The last results.
    #[must_use]
    pub fn results(&self) -> Option<&SearchResults> {
        self.results.as_ref()
    }

    /// Whether a search is going on.
    #[must_use]
    pub fn is_searching(&self) -> bool {
        self.searching.is_some()
    }

    /// The connections the search covers.
    #[must_use]
    pub fn scopes(&self) -> &[SearchScope] {
        &self.scopes
    }

    /// Also match comments.
    pub fn set_in_comments(&mut self, on: bool, cx: &mut Context<Self>) {
        self.in_comments = on;
        self.search(cx);
    }

    /// Also match loaded definitions.
    pub fn set_in_definitions(&mut self, on: bool, cx: &mut Context<Self>) {
        self.in_definitions = on;
        self.search(cx);
    }

    /// Includes or leaves out a connection.
    pub fn set_scope(&mut self, id: ConnectionId, selected: bool, cx: &mut Context<Self>) {
        if let Some(s) = self.scopes.iter_mut().find(|s| s.id == id) {
            s.selected = selected;
        }
        self.search(cx);
    }

    /// Selects result `ix`.
    pub fn select(&mut self, ix: usize, cx: &mut Context<Self>) {
        let len = self.results.as_ref().map_or(0, |r| r.hits.len());
        if len > 0 {
            self.selected = ix.min(len - 1);
        }
        cx.notify();
    }

    /// The selected result.
    #[must_use]
    pub fn selected_hit(&self) -> Option<&SearchHit> {
        self.results.as_ref()?.hits.get(self.selected)
    }

    /// Runs the search for the current query and options. Results of an older search are
    /// dropped.
    pub fn search(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        let query = self.query.read(cx).value().trim().to_owned();
        let connections: Vec<ConnectionId> = self
            .scopes
            .iter()
            .filter(|s| s.selected)
            .map(|s| s.id)
            .collect();
        if query.is_empty() || connections.is_empty() {
            self.results = None;
            self.searching = None;
            cx.notify();
            return;
        }
        let request = SearchRequest::new(query)
            .in_connections(connections)
            .with_comments(self.in_comments)
            .with_definitions(self.in_definitions)
            .with_limit(RESULT_LIMIT);
        let reply = self.service.search(request);
        let generation = self.generation;
        self.searching = Some(cx.spawn(async move |this, cx| {
            let results = reply.await;
            let _ = this.update(cx, |d, cx| {
                if d.generation != generation {
                    return;
                }
                d.searching = None;
                d.results = Some(results.unwrap_or_default());
                d.selected = 0;
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// Loads, in the background and within limits, the metadata of the selected connections,
    /// so search finds more. Cancelled when the dialog closes.
    pub fn load_metadata(&mut self, cx: &mut Context<Self>) {
        self.prefetches.clear();
        for scope in self.scopes.iter().filter(|s| s.selected) {
            self.prefetches.push(self.service.prefetch(
                scope.id,
                ObjectPath::root(),
                PrefetchLimits::new(256, 4),
            ));
        }
        cx.notify();
    }

    /// Chooses the selected result: reveal it (`open = false`) or open its object editor.
    pub fn choose(&mut self, open: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(hit) = self.selected_hit().cloned() else {
            return;
        };
        let choice = if open {
            FindChoice::Open(hit.connection, hit.path)
        } else {
            FindChoice::Reveal(hit.connection, hit.path)
        };
        window.close_dialog(cx);
        (self.on_choose)(choice, window, cx);
    }

    fn scope_name(&self, id: ConnectionId) -> &str {
        self.scopes
            .iter()
            .find(|s| s.id == id)
            .map_or("", |s| s.name.as_str())
    }

    fn on_key(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let len = self.results.as_ref().map_or(0, |r| r.hits.len());
        match event.keystroke.key.as_str() {
            "down" if len > 0 => {
                self.select((self.selected + 1).min(len - 1), cx);
                cx.stop_propagation();
            }
            "up" if len > 0 => {
                self.select(self.selected.saturating_sub(1), cx);
                cx.stop_propagation();
            }
            _ => {}
        }
    }
}

impl Render for FindObjectDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let active = theme.list_active;
        let options = h_flex()
            .gap_4()
            .flex_wrap()
            .child(
                Checkbox::new("find-comments")
                    .checked(self.in_comments)
                    .label("Search comments")
                    .on_click(cx.listener(|this, _, _, cx| {
                        let on = !this.in_comments;
                        this.set_in_comments(on, cx);
                    })),
            )
            .child(
                Checkbox::new("find-definitions")
                    .checked(self.in_definitions)
                    .label("Search definitions (loaded DDL)")
                    .on_click(cx.listener(|this, _, _, cx| {
                        let on = !this.in_definitions;
                        this.set_in_definitions(on, cx);
                    })),
            );
        let scopes = h_flex()
            .gap_4()
            .flex_wrap()
            .children(self.scopes.iter().enumerate().map(|(ix, s)| {
                let id = s.id;
                let selected = s.selected;
                Checkbox::new(("find-scope", ix))
                    .checked(selected)
                    .label(super::display_name(&s.name))
                    .on_click(cx.listener(move |this, _, _, cx| this.set_scope(id, !selected, cx)))
            }));
        let hits = self
            .results
            .as_ref()
            .map(|r| r.hits.clone())
            .unwrap_or_default();
        let rows = hits.iter().enumerate().map(|(ix, hit)| {
            let kind = hit.kind().map_or("object", kind_singular);
            let path = display_path(&hit.path);
            let connection = super::display_name(self.scope_name(hit.connection));
            h_flex()
                .id(("find-hit", ix))
                .w_full()
                .px_2()
                .py_1()
                .gap_3()
                .rounded_sm()
                .when(ix == self.selected, |el| el.bg(active))
                .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                    this.select(ix, cx);
                    this.choose(event.click_count() >= 2, window, cx);
                }))
                .child(
                    v_flex()
                        .flex_1()
                        .overflow_hidden()
                        .child(div().text_sm().child(super::display_name(hit.name())))
                        .child(div().text_xs().text_color(muted).child(path)),
                )
                .child(div().text_xs().text_color(muted).child(kind))
                .child(div().text_xs().text_color(muted).child(connection))
                .child(
                    Button::new(("find-open", ix))
                        .ghost()
                        .xsmall()
                        .label("Open")
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.select(ix, cx);
                            this.choose(true, window, cx);
                        })),
                )
        });
        let status = match &self.results {
            _ if self.scopes.is_empty() => {
                "No connection is open: connect first, then expand some nodes or use \"Load metadata\".".to_owned()
            }
            None => "Search looks at the metadata loaded so far (expanded nodes, or \"Load metadata\").".to_owned(),
            Some(r) if r.hits.is_empty() => "No loaded object matches.".to_owned(),
            Some(r) => {
                let mut s = format!("{} found", r.hits.len());
                if r.truncated {
                    s.push_str(" (search stopped early: refine the query)");
                }
                s
            }
        };
        let loading = !self.prefetches.is_empty();
        v_flex()
            .id("find-object")
            .gap_2()
            .capture_key_down(cx.listener(Self::on_key))
            .child(Input::new(&self.query))
            .child(options)
            .when(self.scopes.len() > 1, |el| el.child(scopes))
            .child(
                h_flex()
                    .justify_between()
                    .child(div().text_xs().text_color(muted).child(status))
                    .child(
                        Button::new("find-load")
                            .ghost()
                            .xsmall()
                            .label(if loading {
                                "Loading metadata…"
                            } else {
                                "Load metadata"
                            })
                            .on_click(cx.listener(|this, _, _, cx| this.load_metadata(cx))),
                    ),
            )
            .child(
                v_flex()
                    .id("find-results")
                    .max_h(px(360.))
                    .overflow_y_scroll()
                    .children(rows),
            )
    }
}

#[cfg(test)]
thread_local! {
    /// The "Find object" dialog opened last on this thread, for tests that open it through its
    /// action (the workspace keeps no handle; GPUI test contexts run on the test's thread).
    pub(crate) static OPENED_FIND: std::cell::RefCell<Option<Entity<FindObjectDialog>>> =
        const { std::cell::RefCell::new(None) };
}

/// Opens "Find object" over `window`.
pub fn open_find_object(
    service: MetadataService,
    scopes: Vec<SearchScope>,
    on_choose: OnChoose,
    window: &mut Window,
    cx: &mut App,
) -> Entity<FindObjectDialog> {
    let view = cx.new(|cx| FindObjectDialog::new(service, scopes, on_choose, window, cx));
    #[cfg(test)]
    OPENED_FIND.with(|o| *o.borrow_mut() = Some(view.clone()));
    let shown = view.clone();
    let first = std::cell::Cell::new(true);
    window.open_dialog(cx, move |dialog, window, cx| {
        if first.replace(false) {
            let input = shown.read(cx).query.clone();
            input.update(cx, |s, cx| s.focus(window, cx));
        }
        dialog.title("Find object").w(px(640.)).child(shown.clone())
    });
    view
}
