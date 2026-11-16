//! P5-A parts of the results view: view modes (grid, record, text), the side panels (value,
//! aggregate, metadata), the client-side sort / filter bar and the advanced copy.
//!
//! Every computation reads the rows the grid holds (I6) and is cached per (result tab, rows
//! version, loaded rows), so a frame never recomputes a panel.

use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, AppContext as _, ClipboardItem, Context, Entity, Image, ImageFormat,
    InteractiveElement, IntoElement, ParentElement, SharedString, StatefulInteractiveElement,
    Styled, Window, div, img, prelude::FluentBuilder, px, uniform_list,
};
use ninesql_core::driver_api::Dialect;

use super::{GridDelegate, ResultsView};
use crate::advanced_copy::{
    AdvancedCopy, AdvancedFormat, CopyError, InsertTarget, copy_advanced, parse_table_name,
};
use crate::aggregate::{Aggregates, aggregate, number_text};
use crate::copy::CopyOutput;
use crate::execution::ResultTab;
use crate::inspect::{ColumnInfoRow, RecordField, TextView, column_info, record, text_view};
use crate::rows::{
    ClientFilter, FilterOp, ServerSortFilter, SortDirection, SortKey, parse_filter_text,
};
use crate::selection::GridSelection;
use crate::viewer::{
    CellInspection, ImageKind, ViewerKind, image_for_preview, inspect_cell, render,
};

/// How the active result is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ViewMode {
    /// The grid.
    #[default]
    Grid,
    /// The focused row as a form.
    Record,
    /// The loaded rows as plain text.
    Text,
}

/// The panel beside the grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidePanel {
    /// Value viewers of the focused cell.
    Value,
    /// Aggregates of the selection.
    Aggregate,
    /// Columns of the result.
    Metadata,
}

type CacheKey = (usize, u64, usize);
/// Focused cell (model row, model column).
type Focus = Option<(usize, usize)>;

/// Cached panel contents.
#[derive(Default)]
pub(super) struct PanelCache {
    inspection: Option<(CacheKey, Focus, Option<CellInspection>)>,
    aggregates: Option<(CacheKey, Aggregates)>,
    text: Option<(CacheKey, Rc<TextView>)>,
    image: Option<(CacheKey, Focus, Arc<Image>)>,
}

/// The P5-A state of the results view.
#[derive(Default)]
pub(super) struct P5State {
    pub(super) mode: ViewMode,
    pub(super) panel: Option<SidePanel>,
    pub(super) viewer: Option<ViewerKind>,
    pub(super) dialect: Option<Arc<dyn Dialect>>,
    pub(super) source_table: Option<Vec<String>>,
    pub(super) server: Option<Rc<dyn ServerSortFilter>>,
    pub(super) filter_input: Option<Entity<InputState>>,
    pub(super) filter_column: Option<usize>,
    pub(super) cache: PanelCache,
    pub(super) subscriptions: Vec<gpui_kit::Subscription>,
}

impl ResultsView {
    /// How the active result is shown.
    #[must_use]
    pub fn mode(&self) -> ViewMode {
        self.p5.mode
    }

    /// Shows the active result as a grid, a record or text.
    pub fn set_mode(&mut self, mode: ViewMode, cx: &mut Context<Self>) {
        self.p5.mode = mode;
        cx.notify();
    }

    /// The side panel shown.
    #[must_use]
    pub fn panel(&self) -> Option<SidePanel> {
        self.p5.panel
    }

    /// Shows `panel`, or hides it when it is the one shown.
    pub fn toggle_panel(&mut self, panel: SidePanel, cx: &mut Context<Self>) {
        self.p5.panel = if self.p5.panel == Some(panel) {
            None
        } else {
            Some(panel)
        };
        cx.notify();
    }

    /// The session dialect, for "Copy as SQL INSERT" (set by the editor tab).
    pub fn set_dialect(&mut self, dialect: Option<Arc<dyn Dialect>>) {
        self.p5.dialect = dialect;
    }

    /// The table the result reads, when known (navigator context): the target of "Copy as SQL
    /// INSERT" and the source column of the metadata panel.
    pub fn set_source_table(&mut self, table: Option<Vec<String>>) {
        self.p5.source_table = table;
    }

    /// The table the result reads, when known.
    #[must_use]
    pub fn source_table(&self) -> Option<&[String]> {
        self.p5.source_table.as_deref()
    }

    /// The server-side sort / filter hook (the core's, once it exists).
    pub fn set_server_sort_filter(&mut self, server: Option<Rc<dyn ServerSortFilter>>) {
        self.p5.server = server;
    }

    /// Whether a server-side sort / filter is offered for the active result (I8).
    #[must_use]
    pub fn server_sort_filter_available(&self) -> bool {
        self.p5.server.as_ref().is_some_and(|s| s.available())
    }

    fn active_delegate<R>(
        &self,
        cx: &gpui_kit::App,
        f: impl FnOnce(&GridDelegate) -> R,
    ) -> Option<R> {
        let table = self.active_table()?;
        Some(f(table.read(cx).delegate()))
    }

    fn update_delegate(
        &mut self,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut GridDelegate),
    ) -> bool {
        let Some(table) = self.active_table().cloned() else {
            return false;
        };
        table.update(cx, |t, cx| {
            f(t.delegate_mut());
            t.refresh(cx);
            cx.notify();
        });
        cx.notify();
        true
    }

    fn cache_key(&self, cx: &gpui_kit::App) -> Option<CacheKey> {
        let ix = self.active_index()?;
        self.active_delegate(cx, |d| (ix, d.version(), d.model().borrow().row_count()))
    }

    /// The label of the client-side sort / filter of the active result ("Loaded rows only…").
    #[must_use]
    pub fn rows_label(&self, cx: &gpui_kit::App) -> Option<String> {
        self.active_delegate(cx, |d| d.rows().label(&d.model().borrow()))
            .flatten()
    }

    /// Display rows of the active result (after the client-side filter).
    #[must_use]
    pub fn shown_rows(&self, cx: &gpui_kit::App) -> Option<usize> {
        self.active_delegate(cx, |d| d.rows().len(d.model().borrow().row_count()))
    }

    /// The selection of the active result (display coordinates).
    #[must_use]
    pub fn selection(&self, cx: &gpui_kit::App) -> Option<GridSelection> {
        self.active_delegate(cx, GridDelegate::selection)
    }

    /// Sets the selection of the active result (display coordinates).
    pub fn select(&mut self, selection: GridSelection, cx: &mut Context<Self>) {
        self.update_delegate(cx, |d| d.set_selection(selection));
    }

    /// Sorts the loaded rows by the focused column.
    pub fn sort_focused(&mut self, direction: SortDirection, cx: &mut Context<Self>) -> bool {
        let Some(col) = self
            .active_delegate(cx, |d| {
                d.selection().focus_col().and_then(|c| d.order().model(c))
            })
            .flatten()
        else {
            self.status = Some("Select a cell or a column to sort by.".into());
            cx.notify();
            return false;
        };
        self.update_delegate(cx, |d| {
            d.update_rows(|rows, m| {
                rows.set_sort(
                    Some(SortKey {
                        column: col,
                        direction,
                    }),
                    m,
                );
            });
        })
    }

    /// Adds a client-side filter on the active result.
    pub fn add_client_filter(&mut self, filter: ClientFilter, cx: &mut Context<Self>) -> bool {
        self.update_delegate(cx, |d| d.update_rows(|rows, m| rows.add_filter(filter, m)))
    }

    /// Keeps the loaded rows whose focused-column value equals the focused cell's (`is null`
    /// for a NULL).
    pub fn filter_by_focused_value(&mut self, cx: &mut Context<Self>) -> bool {
        let filter = self
            .active_delegate(cx, |d| {
                let (row, col) = d.focused_cell()?;
                let m = d.model().borrow();
                let (array, r) = m.array_at(row, col)?;
                if ninesql_core::driver_api::arrow_array::Array::is_null(array.as_ref(), r) {
                    return Some(ClientFilter::on(col, FilterOp::IsNull, ""));
                }
                let mut text = String::new();
                crate::format::write_copy(&mut text, array, r, "");
                Some(ClientFilter::on(col, FilterOp::Equals, text))
            })
            .flatten();
        match filter {
            Some(f) => self.add_client_filter(f, cx),
            None => {
                self.status = Some("Select a cell to filter by its value.".into());
                cx.notify();
                false
            }
        }
    }

    /// Removes the client-side sort and filters.
    pub fn clear_sort_filter(&mut self, cx: &mut Context<Self>) {
        self.p5.filter_input = None;
        self.update_delegate(cx, |d| d.update_rows(|rows, _| rows.clear()));
    }

    /// Copies the selection in `format` to the clipboard. SQL `INSERT` uses the dialect and
    /// the source table set by the owner (see [`Self::copy_as_insert_into`] otherwise).
    ///
    /// # Errors
    /// [`CopyError`] (also shown in the status line).
    pub fn copy_as(
        &mut self,
        format: AdvancedFormat,
        cx: &mut Context<Self>,
    ) -> Result<CopyOutput, CopyError> {
        let table = self.p5.source_table.clone();
        self.copy_with_table(format, table, cx)
    }

    /// "Copy as SQL INSERT" into `table` (the name the user gave).
    ///
    /// # Errors
    /// [`CopyError`].
    pub fn copy_as_insert_into(
        &mut self,
        table: Vec<String>,
        cx: &mut Context<Self>,
    ) -> Result<CopyOutput, CopyError> {
        self.copy_with_table(AdvancedFormat::SqlInsert, Some(table), cx)
    }

    fn copy_with_table(
        &mut self,
        format: AdvancedFormat,
        table: Option<Vec<String>>,
        cx: &mut Context<Self>,
    ) -> Result<CopyOutput, CopyError> {
        let target = match (format, &self.p5.dialect, table) {
            (AdvancedFormat::SqlInsert, Some(dialect), Some(table)) => Some(InsertTarget {
                dialect: dialect.clone(),
                table,
            }),
            _ => None,
        };
        let options = AdvancedCopy::new(format).with_target(target);
        let result = self
            .active_delegate(cx, |d| {
                copy_advanced(
                    &d.model().borrow(),
                    d.order(),
                    d.rows(),
                    &d.selection(),
                    &options,
                )
            })
            .unwrap_or(Err(CopyError::NothingSelected));
        match &result {
            Ok(out) => {
                cx.write_to_clipboard(ClipboardItem::new_string(out.text.clone()));
                let mut status = format!(
                    "Copied {} rows × {} columns as {}",
                    out.rows,
                    out.cols,
                    format.label()
                );
                if out.capped {
                    status.push_str(" (stopped at the clipboard size limit)");
                }
                if out.cut_values {
                    status.push_str("; some values were cut by the value size limit");
                }
                self.status = Some(status);
                cx.emit(super::GridEvent::Copied {
                    rows: out.rows,
                    cols: out.cols,
                    capped: out.capped,
                    cut_values: out.cut_values,
                });
            }
            Err(e) => self.status = Some(e.to_string()),
        }
        cx.notify();
        result
    }

    /// "Copy as SQL INSERT" from the key binding: with a known table it copies; otherwise it
    /// asks for the table name.
    pub(super) fn copy_insert_or_ask(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.p5.dialect.is_none() {
            self.status = Some(
                "Copy as SQL INSERT needs the session's dialect: run the query on a connection."
                    .into(),
            );
            cx.notify();
            return;
        }
        if self.p5.source_table.is_some() {
            let _ = self.copy_as(AdvancedFormat::SqlInsert, cx);
            return;
        }
        let input = cx
            .new(|cx| InputState::new(window, cx).placeholder("Table name: table or schema.table"));
        let this = cx.entity().downgrade();
        let error: Rc<std::cell::RefCell<Option<&'static str>>> = Rc::default();
        window.open_dialog(cx, move |dialog, _window, cx| {
            let input = input.clone();
            let this = this.clone();
            let error = error.clone();
            let danger = cx.theme().danger;
            let message = *error.borrow();
            dialog
                .title("Copy as SQL INSERT")
                .w(px(460.))
                .child(
                    v_flex()
                        .gap_2()
                        .text_sm()
                        .child("The rows are copied as INSERT statements into this table. They are not run.")
                        .child(Input::new(&input))
                        .when_some(message, |d, m| d.child(div().text_color(danger).child(m))),
                )
                .footer(
                    h_flex()
                        .gap_2()
                        .justify_end()
                        .child(
                            gpui_kit::component::button::Button::new("copy-insert-cancel")
                                .label("Cancel")
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child({
                            use gpui_kit::component::button::ButtonVariants as _;
                            gpui_kit::component::button::Button::new("copy-insert-ok")
                                .primary()
                                .label("Copy")
                                .on_click(move |_, window, cx| {
                                    let text = input.read(cx).value().to_string();
                                    match parse_table_name(&text) {
                                        Ok(table) => {
                                            window.close_dialog(cx);
                                            let _ = this.update(cx, |v, cx| {
                                                let _ = v.copy_as_insert_into(table, cx);
                                            });
                                        }
                                        Err(why) => {
                                            *error.borrow_mut() = Some(why);
                                            window.refresh();
                                        }
                                    }
                                })
                        }),
                )
        });
    }

    /// Opens the filter bar (on the focused column, else every column).
    pub(super) fn open_filter_bar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let column = self
            .active_delegate(cx, |d| {
                d.selection().focus_col().and_then(|c| d.order().model(c))
            })
            .flatten();
        if self.active_table().is_none() {
            return;
        }
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("contains…, = v, != v, > v, < v, is null (loaded rows only)")
        });
        let sub = cx.subscribe_in(
            &input,
            window,
            |this: &mut Self, _, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    this.apply_filter_bar(window, cx);
                }
            },
        );
        self.p5.subscriptions = vec![sub];
        input.update(cx, |s, cx| s.focus(window, cx));
        self.p5.filter_input = Some(input);
        self.p5.filter_column = column;
        cx.notify();
    }

    /// Applies the filter bar's text (Enter) and closes it.
    pub fn apply_filter_bar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(input) = self.p5.filter_input.take() else {
            return;
        };
        let text = input.read(cx).value().to_string();
        if let Some(f) = parse_filter_text(&text, self.p5.filter_column) {
            self.add_client_filter(f, cx);
        }
        let handle = gpui_kit::Focusable::focus_handle(self, cx);
        window.focus(&handle, cx);
        cx.notify();
    }

    /// Whether the filter bar is open.
    #[must_use]
    pub fn filter_bar_open(&self) -> bool {
        self.p5.filter_input.is_some()
    }

    /// The filter bar's input (tests type into it).
    #[must_use]
    pub fn filter_input(&self) -> Option<&Entity<InputState>> {
        self.p5.filter_input.as_ref()
    }

    /// Moves the record view (and the focus) by `delta` rows.
    pub fn step_record(&mut self, delta: isize, cx: &mut Context<Self>) {
        self.update_delegate(cx, |d| {
            let rows = d.rows().len(d.model().borrow().row_count());
            if rows == 0 {
                return;
            }
            let (row, col) = d.selection().focus().unwrap_or((0, 0));
            let next = row.saturating_add_signed(delta).min(rows - 1);
            d.set_selection(GridSelection::cell(next, col));
        });
    }

    /// The record view of the focused row (the first row when nothing is selected).
    #[must_use]
    pub fn record_fields(&self, cx: &gpui_kit::App) -> Vec<RecordField> {
        self.active_delegate(cx, |d| {
            let display = d.selection().focus().map_or(0, |(r, _)| r);
            let Some(row) = d.model_row(display) else {
                return Vec::new();
            };
            record(&d.model().borrow(), d.order(), row)
        })
        .unwrap_or_default()
    }

    /// The text view of the active result (cached).
    pub fn text_view(&mut self, cx: &gpui_kit::App) -> Option<Rc<TextView>> {
        let key = self.cache_key(cx)?;
        if let Some((k, t)) = &self.p5.cache.text
            && *k == key
        {
            return Some(t.clone());
        }
        let t = Rc::new(
            self.active_delegate(cx, |d| text_view(&d.model().borrow(), d.order(), d.rows()))?,
        );
        self.p5.cache.text = Some((key, t.clone()));
        Some(t)
    }

    /// The focused cell, read for the value panel (cached).
    pub fn inspection(&mut self, cx: &gpui_kit::App) -> Option<CellInspection> {
        let key = self.cache_key(cx)?;
        let focused = self.active_delegate(cx, GridDelegate::focused_cell)?;
        if let Some((k, f, i)) = &self.p5.cache.inspection
            && *k == key
            && *f == focused
        {
            return i.clone();
        }
        let inspection = focused.and_then(|(r, c)| {
            self.active_delegate(cx, |d| inspect_cell(&d.model().borrow(), r, c))
                .flatten()
        });
        self.p5.cache.inspection = Some((key, focused, inspection.clone()));
        inspection
    }

    /// The viewer the value panel shows for `inspection`: the chosen one when offered, else the
    /// default.
    #[must_use]
    pub fn viewer_for(&self, inspection: &CellInspection) -> ViewerKind {
        self.p5
            .viewer
            .filter(|v| inspection.viewers.contains(v))
            .or_else(|| inspection.viewers.first().copied())
            .unwrap_or(ViewerKind::Text)
    }

    /// Shows the next viewer offered for the focused cell.
    pub fn next_viewer(&mut self, cx: &mut Context<Self>) {
        if self.p5.panel != Some(SidePanel::Value) {
            self.p5.panel = Some(SidePanel::Value);
        }
        if let Some(i) = self.inspection(cx) {
            let current = self.viewer_for(&i);
            let pos = i.viewers.iter().position(|v| *v == current).unwrap_or(0);
            self.p5.viewer = i.viewers.get((pos + 1) % i.viewers.len().max(1)).copied();
        }
        cx.notify();
    }

    /// Aggregates of the selection (cached).
    pub fn aggregates(&mut self, cx: &gpui_kit::App) -> Option<Aggregates> {
        let key = self.cache_key(cx)?;
        if let Some((k, a)) = &self.p5.cache.aggregates
            && *k == key
        {
            return Some(a.clone());
        }
        let a = self.active_delegate(cx, |d| {
            aggregate(&d.model().borrow(), d.order(), d.rows(), &d.selection())
        })?;
        self.p5.cache.aggregates = Some((key, a.clone()));
        Some(a)
    }

    /// The metadata panel of the active result.
    #[must_use]
    pub fn column_info(&self, cx: &gpui_kit::App) -> Vec<ColumnInfoRow> {
        let ix = self.active_index();
        let source = self.p5.source_table.as_ref().map(|t| t.join("."));
        match ix.and_then(|ix| self.results.tabs().get(ix)) {
            Some(ResultTab::Table(m)) => column_info(&m.borrow(), source.as_deref()),
            _ => {
                let _ = cx;
                Vec::new()
            }
        }
    }

    fn image(&mut self, inspection: &CellInspection, cx: &gpui_kit::App) -> Option<Arc<Image>> {
        let key = self.cache_key(cx)?;
        let focused = self.active_delegate(cx, GridDelegate::focused_cell)?;
        if let Some((k, f, i)) = &self.p5.cache.image
            && *k == key
            && *f == focused
        {
            return Some(i.clone());
        }
        let info = image_for_preview(inspection).ok()?;
        let format = match info.kind {
            ImageKind::Png => ImageFormat::Png,
            ImageKind::Jpeg => ImageFormat::Jpeg,
            ImageKind::Gif => ImageFormat::Gif,
        };
        let image = Arc::new(Image::from_bytes(format, inspection.bytes.clone()?));
        self.p5.cache.image = Some((key, focused, image.clone()));
        Some(image)
    }

    // --- rendering ---------------------------------------------------------------------------

    /// The main area of a result tab in the current mode.
    pub(super) fn render_main(&mut self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        match self.p5.mode {
            ViewMode::Grid => div()
                .size_full()
                .when_some(self.table(ix), |d, table| {
                    d.child(
                        gpui_kit::component::table::DataTable::new(table)
                            .stripe(true)
                            .accessibility_label("Query results"),
                    )
                })
                .into_any_element(),
            ViewMode::Record => self.render_record(cx).into_any_element(),
            ViewMode::Text => self.render_text(cx).into_any_element(),
        }
    }

    fn render_record(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let fields = self.record_fields(cx);
        let position = self
            .active_delegate(cx, |d| {
                let rows = d.rows().len(d.model().borrow().row_count());
                let at = d.selection().focus().map_or(0, |(r, _)| r);
                (at + 1, rows)
            })
            .unwrap_or((0, 0));
        v_flex()
            .id("record-view")
            .size_full()
            .p_2()
            .gap_1()
            .overflow_y_scroll()
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(format!(
                        "Record {} of {} (Next record / Previous record)",
                        position.0, position.1
                    )),
            )
            .children(fields.into_iter().map(|f| {
                h_flex()
                    .gap_2()
                    .items_start()
                    .child(
                        v_flex()
                            .w(px(200.))
                            .flex_none()
                            .child(div().font_weight(gpui_kit::FontWeight::BOLD).child(f.name))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(f.type_name),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .when(f.is_null, |d| d.italic().text_color(theme.muted_foreground))
                            .child(f.text)
                            .when(f.cut, |d| {
                                d.child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.warning)
                                        .child("cut by the driver: this is a prefix"),
                                )
                            }),
                    )
            }))
    }

    fn render_text(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let Some(view) = self.text_view(cx) else {
            return div().into_any_element();
        };
        let lines: Rc<Vec<SharedString>> = Rc::new(
            view.text
                .lines()
                .map(|l| SharedString::from(l.to_owned()))
                .collect(),
        );
        let count = lines.len();
        v_flex()
            .size_full()
            .when(view.capped, |d| {
                d.child(div().px_2().bg(theme.muted).text_sm().child(format!(
                    "Text view: first {} of {} rows (bounded).",
                    view.rows, view.total_rows
                )))
            })
            .child(
                uniform_list("text-view", count, move |range, _, _| {
                    range
                        .filter_map(|i| {
                            lines.get(i).map(|l| {
                                div()
                                    .id(("text-line", i))
                                    .font_family("monospace")
                                    .whitespace_nowrap()
                                    .child(l.clone())
                            })
                        })
                        .collect()
                })
                .flex_1()
                .px_2(),
            )
            .into_any_element()
    }

    /// The side panel, if one is shown.
    pub(super) fn render_panel(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let panel = self.p5.panel?;
        let theme = cx.theme().clone();
        let title = match panel {
            SidePanel::Value => "Value",
            SidePanel::Aggregate => "Aggregates",
            SidePanel::Metadata => "Columns",
        };
        let body = match panel {
            SidePanel::Value => self.render_value_panel(cx),
            SidePanel::Aggregate => self.render_aggregate_panel(cx),
            SidePanel::Metadata => self.render_metadata_panel(cx),
        };
        Some(
            v_flex()
                .id("side-panel")
                .w(px(340.))
                .flex_none()
                .h_full()
                .border_l_1()
                .border_color(theme.border)
                .child(
                    div()
                        .px_2()
                        .py_1()
                        .border_b_1()
                        .border_color(theme.border)
                        .font_weight(gpui_kit::FontWeight::BOLD)
                        .child(title),
                )
                .child(
                    div()
                        .id("side-panel-body")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .p_2()
                        .text_sm()
                        .child(body),
                )
                .into_any_element(),
        )
    }

    fn render_value_panel(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let Some(inspection) = self.inspection(cx) else {
            return div()
                .text_color(theme.muted_foreground)
                .child("Select a cell to see its value.")
                .into_any_element();
        };
        let kind = self.viewer_for(&inspection);
        let output = render(&inspection, kind);
        let image = (kind == ViewerKind::Image)
            .then(|| self.image(&inspection, cx))
            .flatten();
        let tabs = inspection.viewers.iter().map(|v| {
            let v = *v;
            div()
                .id(SharedString::from(format!("viewer-{}", v.label())))
                .px_2()
                .rounded_sm()
                .cursor_pointer()
                .when(v == kind, |d| {
                    d.bg(theme.tab_active)
                        .text_color(theme.tab_active_foreground)
                })
                .child(v.label())
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.p5.viewer = Some(v);
                    cx.notify();
                }))
        });
        v_flex()
            .gap_1()
            .child(div().text_color(theme.muted_foreground).child(format!(
                "{} · {}",
                crate::format::display_line(&inspection.column, 60).0,
                inspection.type_name
            )))
            .child(h_flex().gap_1().children(tabs))
            .children(
                output
                    .notes
                    .into_iter()
                    .map(|n| div().text_xs().text_color(theme.warning).child(n)),
            )
            .when_some(image, |d, image| {
                d.child(img(image).max_w(px(320.)).max_h(px(480.)))
            })
            .when(!output.text.is_empty(), |d| {
                d.child(
                    div()
                        .font_family("monospace")
                        .text_xs()
                        .when(inspection.is_null, |d| {
                            d.italic().text_color(theme.muted_foreground)
                        })
                        .child(output.text),
                )
            })
            .into_any_element()
    }

    fn render_aggregate_panel(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let Some(a) = self.aggregates(cx).filter(|a| !a.columns.is_empty()) else {
            return div()
                .text_color(theme.muted_foreground)
                .child("Select cells to aggregate them (loaded rows only).")
                .into_any_element();
        };
        let line = |label: &'static str, value: String| {
            h_flex()
                .gap_2()
                .child(
                    div()
                        .w(px(70.))
                        .text_color(theme.muted_foreground)
                        .child(label),
                )
                .child(value)
        };
        v_flex()
            .gap_2()
            .child(
                div()
                    .text_color(theme.muted_foreground)
                    .child(format!("{} selected rows (loaded rows only)", a.rows)),
            )
            .when(a.capped, |d| {
                d.child(div().text_color(theme.warning).child(format!(
                    "Stopped after {} cells: the aggregates cover the first ones only.",
                    crate::aggregate::MAX_AGGREGATE_CELLS
                )))
            })
            .children(a.columns.into_iter().map(|c| {
                let mut block = v_flex()
                    .gap_0p5()
                    .child(div().font_weight(gpui_kit::FontWeight::BOLD).child(c.name))
                    .child(line("Count", format!("{} of {} cells", c.count, c.cells)));
                if let Some(sum) = c.sum {
                    let text = c.sum_text.unwrap_or_else(|| number_text(sum));
                    block = block.child(line(
                        "Sum",
                        if c.approximate {
                            format!("≈ {text}")
                        } else {
                            text
                        },
                    ));
                }
                if let Some(avg) = c.avg {
                    block = block.child(line("Average", format!("≈ {}", number_text(avg))));
                }
                if let Some(min) = c.min {
                    block = block.child(line("Min", min));
                }
                if let Some(max) = c.max {
                    block = block.child(line("Max", max));
                }
                if c.cut_values {
                    block = block.child(
                        div()
                            .text_xs()
                            .text_color(theme.warning)
                            .child("Values cut by the driver: min / max are prefixes."),
                    );
                }
                block
            }))
            .into_any_element()
    }

    fn render_metadata_panel(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let rows = self.column_info(cx);
        v_flex()
            .gap_2()
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("Nullability as the driver reports it in the result."),
            )
            .children(rows.into_iter().map(|r| {
                v_flex()
                    .gap_0p5()
                    .child(
                        div()
                            .font_weight(gpui_kit::FontWeight::BOLD)
                            .child(format!("{}. {}", r.position, r.name)),
                    )
                    .child(format!(
                        "Type: {}{}",
                        r.engine_type
                            .clone()
                            .unwrap_or_else(|| r.arrow_type.clone()),
                        if r.engine_type.is_some() {
                            format!(" ({})", r.arrow_type)
                        } else {
                            String::new()
                        }
                    ))
                    .child(format!(
                        "Nullable: {}",
                        if r.nullable { "yes" } else { "no" }
                    ))
                    .child(format!(
                        "Source: {}",
                        r.source.unwrap_or_else(|| "unknown".to_owned())
                    ))
                    .when(r.cut, |d| {
                        d.child(
                            div()
                                .text_color(theme.warning)
                                .child("Values cut by the driver (read-only)."),
                        )
                    })
            }))
            .into_any_element()
    }

    /// The filter bar and the "loaded rows only" label, when needed.
    pub(super) fn render_rows_bar(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = cx.theme().clone();
        let label = self.rows_label(cx);
        let input = self.p5.filter_input.clone();
        if label.is_none() && input.is_none() {
            return None;
        }
        let column = self.p5.filter_column.and_then(|c| {
            self.active_delegate(cx, |d| {
                d.model()
                    .borrow()
                    .columns()
                    .get(c)
                    .map(|m| crate::format::display_line(&m.name, 40).0)
            })
            .flatten()
        });
        Some(
            v_flex()
                .id("rows-bar")
                .w_full()
                .px_2()
                .py_1()
                .gap_1()
                .bg(theme.muted)
                .text_sm()
                .when_some(input, |d, input| {
                    d.child(
                        h_flex()
                            .gap_2()
                            .child(format!(
                                "Filter {}:",
                                column.unwrap_or_else(|| "any column".to_owned())
                            ))
                            .child(div().flex_1().child(Input::new(&input))),
                    )
                })
                .when_some(label, |d, label| {
                    d.child(
                        h_flex().gap_2().child(div().flex_1().child(label)).child(
                            div()
                                .id("clear-sort-filter")
                                .px_2()
                                .rounded_sm()
                                .border_1()
                                .border_color(theme.border)
                                .cursor_pointer()
                                .child("Clear")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.clear_sort_filter(cx);
                                })),
                        ),
                    )
                })
                .into_any_element(),
        )
    }
}
