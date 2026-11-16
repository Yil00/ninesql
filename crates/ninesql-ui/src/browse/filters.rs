//! Navigator filters (F-NAV-04): a dialog per connection with include and exclude glob patterns
//! (`*`, `?`) for each kind the driver declares (I8), and a case-sensitivity toggle per kind.
//! Patterns are validated with the core's rules (`KindFilter::new`) and the errors are shown
//! on their kind; "Save" calls `MetadataService::set_filters`, which writes `navigator.json`
//! off the UI thread and invalidates the connection's tree (the navigator reloads it).

use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use ninesql_core::driver_api::ObjectKind;
use ninesql_core::metadata::model::kind_name;
use ninesql_core::{ConnectionFilters, ConnectionId, KindFilter, MetadataService};

use super::kind_plural;

/// The patterns of a comma- or newline-separated list, trimmed, empty ones dropped.
#[must_use]
pub fn split_patterns(text: &str) -> Vec<String> {
    text.split([',', '\n'])
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_owned)
        .collect()
}

/// What the dialog holds for one kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KindInput {
    /// The kind.
    pub kind: ObjectKind,
    /// Include patterns, as typed.
    pub include: String,
    /// Exclude patterns, as typed.
    pub exclude: String,
    /// Case-sensitive matching.
    pub case_sensitive: bool,
}

/// Builds the filters of the dialog's inputs, or the error of each kind that does not
/// validate. Kinds with no pattern have no filter.
///
/// # Errors
/// `(kind, message)` for each invalid kind.
pub fn build_filters(inputs: &[KindInput]) -> Result<ConnectionFilters, Vec<(ObjectKind, String)>> {
    let mut filters = ConnectionFilters::new();
    let mut errors = Vec::new();
    for input in inputs {
        let include = split_patterns(&input.include);
        let exclude = split_patterns(&input.exclude);
        if include.is_empty() && exclude.is_empty() {
            continue;
        }
        match KindFilter::new(include, exclude, input.case_sensitive) {
            Ok(filter) => {
                if let Err(e) = filters.set(input.kind, filter) {
                    errors.push((input.kind, e.to_string()));
                }
            }
            Err(e) => errors.push((input.kind, e.to_string())),
        }
    }
    if errors.is_empty() {
        Ok(filters)
    } else {
        Err(errors)
    }
}

/// The inputs showing `filters` for `kinds` (kinds without a stable name cannot be filtered
/// and are left out).
#[must_use]
pub fn inputs_for(kinds: &[ObjectKind], filters: &ConnectionFilters) -> Vec<KindInput> {
    kinds
        .iter()
        .filter(|k| kind_name(**k).is_some())
        .map(|kind| {
            let f = filters.get(*kind);
            let join = |list: &[ninesql_core::metadata::NamePattern]| {
                list.iter()
                    .map(ninesql_core::metadata::NamePattern::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            KindInput {
                kind: *kind,
                include: f.map(|f| join(f.include())).unwrap_or_default(),
                exclude: f.map(|f| join(f.exclude())).unwrap_or_default(),
                case_sensitive: f.is_some_and(KindFilter::case_sensitive),
            }
        })
        .collect()
}

struct Row {
    kind: ObjectKind,
    include: Entity<InputState>,
    exclude: Entity<InputState>,
    case_sensitive: bool,
    error: Option<String>,
}

/// The filter dialog's content.
pub struct FilterDialog {
    connection: ConnectionId,
    connection_name: String,
    service: MetadataService,
    rows: Vec<Row>,
    error: Option<String>,
    saving: bool,
    saved: bool,
}

impl FilterDialog {
    /// The dialog for `connection`, showing its current filters for `kinds`.
    pub fn new(
        service: MetadataService,
        connection: ConnectionId,
        connection_name: String,
        kinds: &[ObjectKind],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let current = service.filters(connection);
        let rows = inputs_for(kinds, &current)
            .into_iter()
            .map(|i| {
                let include = i.include.clone();
                let exclude = i.exclude.clone();
                Row {
                    kind: i.kind,
                    include: cx.new(|cx| {
                        InputState::new(window, cx)
                            .default_value(include)
                            .placeholder("Include: e.g. app_*, audit")
                    }),
                    exclude: cx.new(|cx| {
                        InputState::new(window, cx)
                            .default_value(exclude)
                            .placeholder("Exclude: e.g. tmp_*, *_old")
                    }),
                    case_sensitive: i.case_sensitive,
                    error: None,
                }
            })
            .collect();
        Self {
            connection,
            connection_name,
            service,
            rows,
            error: None,
            saving: false,
            saved: false,
        }
    }

    /// The inputs as typed.
    #[must_use]
    pub fn inputs(&self, cx: &App) -> Vec<KindInput> {
        self.rows
            .iter()
            .map(|r| KindInput {
                kind: r.kind,
                include: r.include.read(cx).value().to_string(),
                exclude: r.exclude.read(cx).value().to_string(),
                case_sensitive: r.case_sensitive,
            })
            .collect()
    }

    /// Sets the patterns of a kind (tests and programmatic use).
    pub fn set_patterns(
        &mut self,
        kind: ObjectKind,
        include: &str,
        exclude: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(row) = self.rows.iter().find(|r| r.kind == kind) {
            let (inc, exc) = (include.to_owned(), exclude.to_owned());
            row.include.update(cx, |s, cx| s.set_value(inc, window, cx));
            row.exclude.update(cx, |s, cx| s.set_value(exc, window, cx));
        }
    }

    /// Kinds shown.
    #[must_use]
    pub fn kinds(&self) -> Vec<ObjectKind> {
        self.rows.iter().map(|r| r.kind).collect()
    }

    /// The error of a kind, if its patterns did not validate.
    #[must_use]
    pub fn kind_error(&self, kind: ObjectKind) -> Option<&str> {
        self.rows
            .iter()
            .find(|r| r.kind == kind)
            .and_then(|r| r.error.as_deref())
    }

    /// The save error, if any.
    #[must_use]
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Whether the last save succeeded.
    #[must_use]
    pub fn saved(&self) -> bool {
        self.saved
    }

    fn toggle_case(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some(row) = self.rows.get_mut(ix) {
            row.case_sensitive = !row.case_sensitive;
        }
        cx.notify();
    }

    /// Validates and saves; the dialog closes once the core saved.
    pub fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        for row in &mut self.rows {
            row.error = None;
        }
        self.error = None;
        let filters = match build_filters(&self.inputs(cx)) {
            Ok(f) => f,
            Err(errors) => {
                for (kind, message) in errors {
                    if let Some(row) = self.rows.iter_mut().find(|r| r.kind == kind) {
                        row.error = Some(message);
                    }
                }
                self.error = Some("Some patterns are not valid: see the messages above.".into());
                cx.notify();
                return;
            }
        };
        self.saving = true;
        let reply = self.service.set_filters(self.connection, filters);
        cx.spawn_in(window, async move |this, cx| {
            let result = reply.await;
            let _ = this.update_in(cx, |d, window, cx| {
                d.saving = false;
                match result {
                    Some(Ok(())) => {
                        d.saved = true;
                        window.close_dialog(cx);
                    }
                    Some(Err(e)) => d.error = Some(e.to_string()),
                    None => {
                        d.error =
                            Some("The filters were not saved: the application is closing.".into())
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
}

impl Render for FilterDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let danger = theme.danger;
        let rows = self.rows.iter().enumerate().map(|(ix, row)| {
            v_flex()
                .gap_1()
                .child(
                    h_flex()
                        .justify_between()
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(kind_plural(row.kind)),
                        )
                        .child(
                            Checkbox::new(("filter-case", ix))
                                .checked(row.case_sensitive)
                                .label("Case-sensitive")
                                .on_click(
                                    cx.listener(move |this, _, _, cx| this.toggle_case(ix, cx)),
                                ),
                        ),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .child(div().flex_1().child(Input::new(&row.include)))
                        .child(div().flex_1().child(Input::new(&row.exclude))),
                )
                .when_some(row.error.clone(), |el, e| {
                    el.child(div().text_xs().text_color(danger).child(e))
                })
        });
        v_flex()
            .id("navigator-filters")
            .gap_3()
            .child(div().text_sm().text_color(muted).child(format!(
                "Objects of \"{}\" shown in the navigator. Patterns use * and ?, separated by commas; an object is shown when it matches an include pattern (or there is none) and no exclude pattern.",
                self.connection_name
            )))
            .when(self.rows.is_empty(), |el| {
                el.child(
                    div()
                        .text_sm()
                        .text_color(muted)
                        .child("This engine lists no objects that can be filtered."),
                )
            })
            .children(rows)
            .when_some(self.error.clone(), |el, e| {
                el.child(div().id("filters-error").text_sm().text_color(danger).child(e))
            })
    }
}

/// Opens the filter dialog of `connection` over `window`.
pub fn open_filter_dialog(
    service: MetadataService,
    connection: ConnectionId,
    connection_name: String,
    kinds: Vec<ObjectKind>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<FilterDialog> {
    let view =
        cx.new(|cx| FilterDialog::new(service, connection, connection_name, &kinds, window, cx));
    let shown = view.clone();
    let saving = Rc::new(view.clone());
    window.open_dialog(cx, move |dialog, _window, cx| {
        let busy = saving.read(cx).saving;
        let save = {
            let view = shown.clone();
            Button::new("filters-save")
                .primary()
                .label("Save")
                .disabled(busy)
                .on_click(move |_, window, cx| view.update(cx, |d, cx| d.save(window, cx)))
        };
        let cancel = Button::new("filters-cancel")
            .label("Cancel")
            .on_click(|_, window, cx| window.close_dialog(cx));
        dialog
            .title("Navigator filters")
            .w(px(640.))
            .child(shown.clone())
            .footer(h_flex().gap_2().justify_end().child(cancel).child(save))
    });
    view
}
