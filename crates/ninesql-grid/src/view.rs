//! The GPUI results view (F-DATA-01, F-SQL-04): one tab per result set, an update counts pane
//! and a notices pane, the truncation banner, the fetch controls and every grid action.
//!
//! A result set is drawn by `gpui-component`'s `DataTable` over a [`GridDelegate`]: the table
//! asks only for the visible rows and columns, and the delegate formats exactly those cells
//! from the Arrow batches ([`ResultSetModel::cell`]). Nothing here awaits I/O (I6): rows come in
//! through [`ResultsView::apply`] and requests go out through the [`ResultSource`].

use std::ops::Range;
use std::rc::Rc;

use gpui_kit::component::table::{Column, DataTable, TableDelegate, TableEvent, TableState};
use gpui_kit::component::{ActiveTheme as _, h_flex, v_flex};
use gpui_kit::{
    App, AppContext as _, ClipboardItem, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, IntoElement, ParentElement, Pixels, Render, ScrollStrategy, SharedString,
    StatefulInteractiveElement, Styled, Subscription, WeakEntity, Window, div,
    prelude::FluentBuilder, px, uniform_list,
};

use crate::actions::{
    AutoSizeAllColumns, AutoSizeColumn, CopySelection, CopySelectionAsCsv,
    CopySelectionWithHeaders, ExtendSelectionDown, ExtendSelectionLeft, ExtendSelectionRight,
    ExtendSelectionUp, FetchAllRows, FetchNextPage, GRID_CONTEXT, NextResultTab, PreviousResultTab,
    SelectAllCells,
};
use crate::copy::{CopyFormat, CopyOptions, CopyOutput, copy_selection};
use crate::execution::{
    ExecutionResults, GridBudget, ResultTab, SharedResult, StatementStatus, format_bytes,
};
use crate::format::{Align, DisplayOptions, ValueKind, display_line};
use crate::model::{FetchState, ResultSetModel};
use crate::selection::{ColumnOrder, GridSelection};
use crate::source::{ExecutionEvent, ResultSource};
use ninesql_core::driver_api::{NoticeSeverity, TruncationReason};

/// Rows sampled by "fit column width" besides the visible ones (bounded work).
pub const AUTO_SIZE_SAMPLE_ROWS: usize = 500;
/// Widest a fitted column gets, in characters.
pub const AUTO_SIZE_MAX_CHARS: usize = 60;
/// Rows from the end of the loaded rows at which scrolling asks for the next page.
pub const LOAD_MORE_THRESHOLD: usize = 50;

/// What the view tells its owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GridEvent {
    /// A copy put text on the clipboard.
    Copied {
        /// Rows copied.
        rows: usize,
        /// Columns copied.
        cols: usize,
        /// The copy size bound stopped it early.
        capped: bool,
        /// Some copied values were cut by the driver.
        cut_values: bool,
    },
}

/// The `TableDelegate` of one tabular result set.
pub struct GridDelegate {
    model: SharedResult,
    order: ColumnOrder,
    selection: GridSelection,
    options: Rc<DisplayOptions>,
    visible_rows: Range<usize>,
    view: WeakEntity<ResultsView>,
    tab: usize,
}

impl GridDelegate {
    fn new(
        model: SharedResult,
        options: Rc<DisplayOptions>,
        view: WeakEntity<ResultsView>,
        tab: usize,
    ) -> Self {
        let cols = model.borrow().columns().len();
        Self {
            model,
            order: ColumnOrder::identity(cols),
            selection: GridSelection::None,
            options,
            visible_rows: 0..0,
            view,
            tab,
        }
    }

    /// The result set drawn.
    #[must_use]
    pub fn model(&self) -> &SharedResult {
        &self.model
    }

    /// Display order of the columns.
    #[must_use]
    pub fn order(&self) -> &ColumnOrder {
        &self.order
    }

    /// The current selection, in display coordinates.
    #[must_use]
    pub fn selection(&self) -> GridSelection {
        self.selection
    }

    /// Rows the table drew last.
    #[must_use]
    pub fn visible_rows(&self) -> Range<usize> {
        self.visible_rows.clone()
    }

    /// Characters needed by display column `col`: header, type and a bounded sample of cells
    /// (visible rows plus the first [`AUTO_SIZE_SAMPLE_ROWS`]).
    #[must_use]
    pub fn content_chars(&self, col: usize) -> usize {
        let Some(c) = self.order.model(col) else {
            return 0;
        };
        let m = self.model.borrow();
        let meta = &m.columns()[c];
        let mut chars = meta
            .header_label()
            .chars()
            .count()
            .max(meta.type_name().chars().count());
        let sample = (0..AUTO_SIZE_SAMPLE_ROWS.min(m.row_count())).chain(self.visible_rows.clone());
        for row in sample {
            if let Some(cell) = m.cell(row, c, &self.options) {
                chars = chars.max(cell.text.chars().count());
            }
            if chars >= AUTO_SIZE_MAX_CHARS {
                return AUTO_SIZE_MAX_CHARS;
            }
        }
        chars
    }

    fn default_width(kind: ValueKind, name_chars: usize) -> Pixels {
        let base: f32 = match kind {
            ValueKind::Number => 100.,
            ValueKind::Boolean => 70.,
            ValueKind::Temporal => 180.,
            ValueKind::Binary | ValueKind::Json | ValueKind::Text => 180.,
            ValueKind::Unsupported => 120.,
        };
        px(base.max(name_chars.min(40) as f32 * 8. + 24.))
    }
}

impl TableDelegate for GridDelegate {
    fn columns_count(&self, _cx: &App) -> usize {
        self.order.len()
    }

    fn rows_count(&self, _cx: &App) -> usize {
        self.model.borrow().row_count()
    }

    fn column(&self, col_ix: usize, _cx: &App) -> Column {
        let m = self.model.borrow();
        let Some(meta) = self.order.model(col_ix).and_then(|c| m.columns().get(c)) else {
            return Column::new("?", "?");
        };
        let column = Column::new(format!("c{col_ix}"), meta.header_label())
            .width(Self::default_width(meta.kind, meta.name.chars().count()))
            .min_width(px(40.))
            .max_width(px(1200.))
            .resizable(true)
            .movable(true);
        if meta.kind.align() == Align::Right {
            column.text_right()
        } else {
            column
        }
    }

    fn render_th(
        &mut self,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let m = self.model.borrow();
        let meta = self.order.model(col_ix).and_then(|c| m.columns().get(c));
        let (label, type_name, cut) = meta.map_or_else(
            || (String::new(), String::new(), false),
            |meta| {
                (
                    meta.name.clone(),
                    meta.type_name().to_owned(),
                    meta.truncated_values,
                )
            },
        );
        let (label, _) = display_line(&label, 80);
        h_flex()
            .w_full()
            .gap_1()
            .overflow_hidden()
            .child(div().truncate().child(label))
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(type_name),
            )
            .when(cut, |d| {
                // Visible marker of values cut by the driver: read-only column (I8).
                d.child(
                    div()
                        .text_xs()
                        .px_1()
                        .rounded_sm()
                        .bg(cx.theme().warning)
                        .text_color(cx.theme().warning_foreground)
                        .child("cut"),
                )
            })
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let m = self.model.borrow();
        let cell = self
            .order
            .model(col_ix)
            .and_then(|c| m.cell(row_ix, c, &self.options));
        let rows = m.row_count();
        drop(m);
        let selected = self.selection.cell_count(rows, self.order.len()) > 1
            && self.selection.row_range(rows).contains(&row_ix)
            && self.selection.col_range(self.order.len()).contains(&col_ix);
        let Some(cell) = cell else {
            return div().into_any_element();
        };
        div()
            .w_full()
            .truncate()
            .when(cell.align == Align::Right, |d| d.text_right())
            .when(cell.align == Align::Center, |d| d.text_center())
            .when(cell.is_null, |d| {
                d.italic().text_color(cx.theme().muted_foreground)
            })
            .when(selected, |d| d.bg(cx.theme().table_active))
            .child(cell.text)
            .into_any_element()
    }

    fn cell_label(&self, row_ix: usize, col_ix: usize, _cx: &App) -> Option<SharedString> {
        let m = self.model.borrow();
        let c = self.order.model(col_ix)?;
        let cell = m.cell(row_ix, c, &self.options)?;
        Some(cell.text.into())
    }

    fn cell_text(&self, row_ix: usize, col_ix: usize, _cx: &App) -> String {
        let mut out = String::new();
        if let Some(c) = self.order.model(col_ix) {
            self.model.borrow().write_cell_copy(&mut out, row_ix, c, "");
        }
        out
    }

    fn move_column(
        &mut self,
        col_ix: usize,
        to_ix: usize,
        _window: &mut Window,
        _cx: &mut Context<TableState<Self>>,
    ) {
        self.order.move_column(col_ix, to_ix);
        self.selection = GridSelection::None;
    }

    fn fit_column(
        &mut self,
        col_ix: usize,
        window: &mut Window,
        _cx: &mut Context<TableState<Self>>,
    ) -> Option<Pixels> {
        let chars = self.content_chars(col_ix).max(3);
        // Average glyph width of the UI font, about 0.55 em; the header also holds the type.
        Some(px(chars as f32 * f32::from(window.rem_size()) * 0.55 + 8.))
    }

    fn has_more(&self, _cx: &App) -> bool {
        self.model.borrow().state().can_fetch()
    }

    fn load_more_threshold(&self) -> usize {
        LOAD_MORE_THRESHOLD
    }

    fn load_more(&mut self, _window: &mut Window, cx: &mut Context<TableState<Self>>) {
        let tab = self.tab;
        // Must not update the table again from here: `fetch` only touches the view and the
        // shared model.
        let _ = self.view.update(cx, |view, cx| view.fetch(tab, false, cx));
    }

    fn visible_rows_changed(
        &mut self,
        visible_range: Range<usize>,
        _window: &mut Window,
        _cx: &mut Context<TableState<Self>>,
    ) {
        self.visible_rows = visible_range;
    }
}

struct TableTab {
    table: Entity<TableState<GridDelegate>>,
    _subscription: Subscription,
}

/// Which pane the view shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivePane {
    /// The result tab at this index.
    Result(usize),
    /// Update counts and statement errors.
    UpdateCounts,
    /// Server notices.
    Notices,
}

/// The results area of one editor tab.
pub struct ResultsView {
    results: ExecutionResults,
    tables: Vec<Option<TableTab>>,
    active: ActivePane,
    source: Option<Rc<dyn ResultSource>>,
    options: Rc<DisplayOptions>,
    focus_handle: FocusHandle,
    /// The last copy or fetch message for the status line.
    status: Option<String>,
}

impl EventEmitter<GridEvent> for ResultsView {}

impl Focusable for ResultsView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match self.active_table() {
            Some(t) => t.read(cx).focus_handle(cx),
            None => self.focus_handle.clone(),
        }
    }
}

impl ResultsView {
    /// An empty view under `budget`.
    pub fn new(budget: GridBudget, cx: &mut Context<Self>) -> Self {
        Self {
            results: ExecutionResults::new(budget),
            tables: Vec::new(),
            active: ActivePane::UpdateCounts,
            source: None,
            options: Rc::new(DisplayOptions::default()),
            focus_handle: cx.focus_handle(),
            status: None,
        }
    }

    /// Connects the view to the execution service.
    pub fn set_source(&mut self, source: Rc<dyn ResultSource>) {
        self.source = Some(source);
    }

    /// Everything the execution produced so far.
    #[must_use]
    pub fn results(&self) -> &ExecutionResults {
        &self.results
    }

    /// The pane shown.
    #[must_use]
    pub fn active_pane(&self) -> ActivePane {
        self.active
    }

    /// The status line message (copy, fetch).
    #[must_use]
    pub fn status(&self) -> Option<&str> {
        self.status.as_deref()
    }

    /// Shows a pane.
    pub fn set_active_pane(&mut self, pane: ActivePane, cx: &mut Context<Self>) {
        if let ActivePane::Result(ix) = pane
            && ix >= self.results.tabs().len()
        {
            return;
        }
        self.active = pane;
        cx.notify();
    }

    /// The table entity of result tab `ix`.
    #[must_use]
    pub fn table(&self, ix: usize) -> Option<&Entity<TableState<GridDelegate>>> {
        self.tables.get(ix)?.as_ref().map(|t| &t.table)
    }

    fn active_table(&self) -> Option<&Entity<TableState<GridDelegate>>> {
        match self.active {
            ActivePane::Result(ix) => self.table(ix),
            _ => None,
        }
    }

    /// Every pane in tab-strip order.
    fn panes(&self) -> Vec<ActivePane> {
        (0..self.results.tabs().len())
            .map(ActivePane::Result)
            .chain([ActivePane::UpdateCounts, ActivePane::Notices])
            .collect()
    }

    /// Applies one execution event (on the foreground thread, from the execution service).
    pub fn apply(&mut self, event: ExecutionEvent, window: &mut Window, cx: &mut Context<Self>) {
        let applied = self.results.apply(event);
        if applied.reset {
            self.tables.clear();
            self.active = ActivePane::UpdateCounts;
            self.status = None;
        }
        for ix in applied.new_tabs {
            let tab = match &self.results.tabs()[ix] {
                ResultTab::Table(model) => Some(self.new_table(ix, model.clone(), window, cx)),
                ResultTab::Documents { .. } => None,
            };
            if self.tables.len() <= ix {
                self.tables.resize_with(ix + 1, || None);
            }
            self.tables[ix] = tab;
            // The first result set is shown as soon as it starts.
            if !matches!(self.active, ActivePane::Result(_)) {
                self.active = ActivePane::Result(ix);
            }
        }
        if let Some(ix) = applied.changed_tab
            && let Some(table) = self.table(ix)
        {
            table.update(cx, |_, cx| cx.notify());
        }
        if let Some(statement) = applied.release
            && let Some(source) = &self.source
        {
            source.release(statement);
        }
        cx.notify();
    }

    fn new_table(
        &mut self,
        ix: usize,
        model: SharedResult,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> TableTab {
        let delegate = GridDelegate::new(model, self.options.clone(), cx.entity().downgrade(), ix);
        let table = cx.new(|cx| {
            TableState::new(delegate, window, cx)
                .cell_selectable(true)
                .row_selectable(true)
                .col_selectable(true)
                .col_resizable(true)
                .col_movable(true)
                .sortable(false)
                .loop_selection(false)
        });
        let subscription = cx.subscribe(&table, |_, table, event: &TableEvent, cx| {
            let selection = match *event {
                TableEvent::SelectCell(r, c) => Some(GridSelection::cell(r, c)),
                TableEvent::SelectRow(r) => Some(GridSelection::Rows {
                    anchor: r,
                    focus: r,
                }),
                TableEvent::SelectColumn(c) => Some(GridSelection::Columns {
                    anchor: c,
                    focus: c,
                }),
                TableEvent::ClearSelection => Some(GridSelection::None),
                _ => None,
            };
            if let Some(selection) = selection {
                table.update(cx, |t, cx| {
                    t.delegate_mut().selection = selection;
                    cx.notify();
                });
            }
        });
        TableTab {
            table,
            _subscription: subscription,
        }
    }

    /// Starts a fetch on result tab `ix` ("fetch next page" or "fetch all") and sends the request.
    /// Does nothing when that result cannot fetch (complete, truncated, closed, a page in flight):
    /// the controls are disabled then (I8).
    pub fn fetch(&mut self, ix: usize, all: bool, cx: &mut Context<Self>) {
        let Some(request) = self.results.request_fetch(ix, all) else {
            return;
        };
        if let Some(source) = &self.source {
            source.fetch(request);
        }
        cx.notify();
    }

    /// Copies the active result's selection to the clipboard.
    pub fn copy(
        &mut self,
        format: CopyFormat,
        headers: bool,
        cx: &mut Context<Self>,
    ) -> Option<CopyOutput> {
        let table = self.active_table()?;
        let out = {
            let t = table.read(cx);
            let d = t.delegate();
            let m = d.model.borrow();
            copy_selection(
                &m,
                &d.order,
                &d.selection,
                &CopyOptions::new(format, headers),
            )?
        };
        cx.write_to_clipboard(ClipboardItem::new_string(out.text.clone()));
        let mut status = format!("Copied {} rows × {} columns", out.rows, out.cols);
        if out.capped {
            status.push_str(" (stopped at the clipboard size limit)");
        }
        if out.cut_values {
            status.push_str("; some values were cut by the value size limit");
        }
        self.status = Some(status);
        cx.emit(GridEvent::Copied {
            rows: out.rows,
            cols: out.cols,
            capped: out.capped,
            cut_values: out.cut_values,
        });
        cx.notify();
        Some(out)
    }

    fn select_all(&mut self, cx: &mut Context<Self>) {
        if let Some(table) = self.active_table() {
            table.update(cx, |t, cx| {
                t.delegate_mut().selection = GridSelection::All;
                cx.notify();
            });
        }
    }

    fn extend(&mut self, d_row: isize, d_col: isize, cx: &mut Context<Self>) {
        let Some(table) = self.active_table() else {
            return;
        };
        table.update(cx, |t, cx| {
            let rows = t.delegate().model.borrow().row_count();
            let cols = t.delegate().order.len();
            let d = t.delegate_mut();
            d.selection.extend(d_row, d_col, rows, cols);
            if let Some((row, col)) = d.selection.focus() {
                t.vertical_scroll_handle
                    .scroll_to_item(row, ScrollStrategy::Nearest);
                t.scroll_to_col(col, cx);
            }
            cx.notify();
        });
    }

    fn auto_size(&mut self, all: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(table) = self.active_table() else {
            return;
        };
        table.update(cx, |t, cx| {
            let cols: Vec<usize> = if all {
                (0..t.delegate().order.len()).collect()
            } else {
                t.delegate().selection.focus_col().into_iter().collect()
            };
            for c in cols {
                t.fit_col(c, window, cx);
            }
        });
    }

    fn cycle(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let panes = self.panes();
        let pos = panes.iter().position(|p| *p == self.active).unwrap_or(0);
        let next = if forward {
            (pos + 1) % panes.len()
        } else {
            (pos + panes.len() - 1) % panes.len()
        };
        self.set_active_pane(panes[next], cx);
        let handle = self.focus_handle(cx);
        window.focus(&handle, cx);
    }

    fn active_index(&self) -> Option<usize> {
        match self.active {
            ActivePane::Result(ix) => Some(ix),
            _ => None,
        }
    }

    /// The banner text for result tab `ix`, if it needs one.
    #[must_use]
    pub fn banner(&self, ix: usize) -> Option<String> {
        match self.results.tabs().get(ix)? {
            ResultTab::Table(m) => banner_for(&m.borrow(), &self.results),
            ResultTab::Documents {
                count, truncated, ..
            } => {
                let mut text = format!(
                    "{count} documents received. The document viewer is not available yet."
                );
                if truncated.is_some() {
                    text.push_str(" Truncated: memory budget reached.");
                }
                Some(text)
            }
        }
    }

    /// The status line of result tab `ix`: rows, memory, fetch state.
    #[must_use]
    pub fn status_line(&self, ix: usize) -> Option<String> {
        let ResultTab::Table(m) = self.results.tabs().get(ix)? else {
            return None;
        };
        let m = m.borrow();
        let state = match m.state() {
            FetchState::Fetching { target_rows: None } => "fetching all rows…",
            FetchState::Fetching { .. } => "fetching…",
            FetchState::Idle => "more rows available",
            FetchState::Complete => "all rows",
            FetchState::Truncated(_) | FetchState::GridBudgetReached => "truncated",
            FetchState::Closed => "closed",
            FetchState::Stopped => "stopped",
        };
        Some(format!(
            "{} rows · {} of {} · {state}",
            m.row_count(),
            format_bytes(self.results.bytes()),
            format_bytes(self.results.budget().max_bytes),
        ))
    }
}

fn banner_for(m: &ResultSetModel, results: &ExecutionResults) -> Option<String> {
    let rows = m.row_count();
    let text = match m.state() {
        FetchState::Truncated(TruncationReason::MaxBytes) | FetchState::GridBudgetReached => {
            format!(
                "Truncated: memory budget reached. {rows} rows kept ({} of {}); narrow the query \
                 to see the rest.",
                format_bytes(results.bytes()),
                format_bytes(results.budget().max_bytes)
            )
        }
        FetchState::Truncated(TruncationReason::MaxRows) => {
            format!("Truncated: row limit reached. {rows} rows kept.")
        }
        FetchState::Closed => format!(
            "The result was closed before its end: {rows} rows loaded. Run the statement again \
             to see more."
        ),
        FetchState::Stopped => {
            format!("Stopped: the statement failed or was cancelled. {rows} rows loaded.")
        }
        _ if m.has_truncated_values() => {
            "Some values were cut at the value size limit: columns marked \"cut\" are read-only."
                .to_owned()
        }
        _ => return None,
    };
    Some(text)
}

impl Render for ResultsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let multi = self.results.multi_statement();
        let mut strip = h_flex()
            .id("result-tabs")
            .w_full()
            .gap_1()
            .px_1()
            .bg(theme.tab_bar)
            .border_b_1()
            .border_color(theme.border)
            .overflow_x_scroll();
        let tab_button = |id: SharedString, label: String, active: bool, pane: ActivePane| {
            div()
                .id(id)
                .px_2()
                .py_0p5()
                .cursor_pointer()
                .when(active, |d| {
                    d.bg(theme.tab_active)
                        .text_color(theme.tab_active_foreground)
                })
                .when(!active, |d| d.text_color(theme.tab_foreground))
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| this.set_active_pane(pane, cx)))
        };
        for (ix, tab) in self.results.tabs().iter().enumerate() {
            let pane = ActivePane::Result(ix);
            strip = strip.child(tab_button(
                format!("result-tab-{ix}").into(),
                tab.label(multi),
                self.active == pane,
                pane,
            ));
        }
        strip = strip
            .child(tab_button(
                "update-counts-tab".into(),
                format!("Update counts ({})", self.results.update_counts().len()),
                self.active == ActivePane::UpdateCounts,
                ActivePane::UpdateCounts,
            ))
            .child(tab_button(
                "notices-tab".into(),
                format!("Notices ({})", self.results.notices().len()),
                self.active == ActivePane::Notices,
                ActivePane::Notices,
            ));

        let body = match self.active {
            ActivePane::Result(ix) => self.render_result(ix, cx).into_any_element(),
            ActivePane::UpdateCounts => self.render_update_counts(cx).into_any_element(),
            ActivePane::Notices => self.render_notices(cx).into_any_element(),
        };

        v_flex()
            .key_context(GRID_CONTEXT)
            .track_focus(&self.focus_handle)
            .size_full()
            .on_action(cx.listener(|this, _: &CopySelection, _, cx| {
                this.copy(CopyFormat::Tsv, false, cx);
            }))
            .on_action(cx.listener(|this, _: &CopySelectionWithHeaders, _, cx| {
                this.copy(CopyFormat::Tsv, true, cx);
            }))
            .on_action(cx.listener(|this, _: &CopySelectionAsCsv, _, cx| {
                this.copy(CopyFormat::Csv, true, cx);
            }))
            .on_action(cx.listener(|this, _: &SelectAllCells, _, cx| this.select_all(cx)))
            .on_action(cx.listener(|this, _: &ExtendSelectionUp, _, cx| this.extend(-1, 0, cx)))
            .on_action(cx.listener(|this, _: &ExtendSelectionDown, _, cx| this.extend(1, 0, cx)))
            .on_action(cx.listener(|this, _: &ExtendSelectionLeft, _, cx| this.extend(0, -1, cx)))
            .on_action(cx.listener(|this, _: &ExtendSelectionRight, _, cx| this.extend(0, 1, cx)))
            .on_action(cx.listener(|this, _: &FetchNextPage, _, cx| {
                if let Some(ix) = this.active_index() {
                    this.fetch(ix, false, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &FetchAllRows, _, cx| {
                if let Some(ix) = this.active_index() {
                    this.fetch(ix, true, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &AutoSizeColumn, window, cx| {
                this.auto_size(false, window, cx);
            }))
            .on_action(cx.listener(|this, _: &AutoSizeAllColumns, window, cx| {
                this.auto_size(true, window, cx);
            }))
            .on_action(cx.listener(|this, _: &NextResultTab, window, cx| {
                this.cycle(true, window, cx);
            }))
            .on_action(cx.listener(|this, _: &PreviousResultTab, window, cx| {
                this.cycle(false, window, cx);
            }))
            .child(strip)
            .child(div().flex_1().min_h_0().child(body))
    }
}

impl ResultsView {
    fn render_result(&self, ix: usize, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let banner = self.banner(ix);
        let budget_banner = match self.results.tabs().get(ix) {
            Some(ResultTab::Table(m)) => m.borrow().state().budget_reached(),
            _ => false,
        };
        let can_fetch = match self.results.tabs().get(ix) {
            Some(ResultTab::Table(m)) => m.borrow().state().can_fetch(),
            _ => false,
        };
        let status = self.status_line(ix);
        let fetch_button = |id: &'static str, label: &'static str, all: bool| {
            div()
                .id(id)
                .px_2()
                .rounded_sm()
                .border_1()
                .border_color(theme.border)
                .when(can_fetch, |d| {
                    d.cursor_pointer()
                        .on_click(cx.listener(move |this, _, _, cx| this.fetch(ix, all, cx)))
                })
                .when(!can_fetch, |d| d.text_color(theme.muted_foreground))
                .child(label)
        };
        v_flex()
            .size_full()
            .when_some(banner, |d, text| {
                d.child(
                    div()
                        .id("result-banner")
                        .w_full()
                        .px_2()
                        .py_1()
                        .when(budget_banner, |d| {
                            d.bg(theme.warning).text_color(theme.warning_foreground)
                        })
                        .when(!budget_banner, |d| {
                            d.bg(theme.muted).text_color(theme.foreground)
                        })
                        .child(text),
                )
            })
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .when_some(self.table(ix), |d, table| {
                        d.child(
                            DataTable::new(table)
                                .stripe(true)
                                .accessibility_label("Query results"),
                        )
                    }),
            )
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .px_2()
                    .text_sm()
                    .border_t_1()
                    .border_color(theme.border)
                    .text_color(theme.muted_foreground)
                    .when_some(status, |d, s| d.child(s))
                    .when_some(self.status.clone(), |d, s| d.child(s))
                    .child(div().flex_1())
                    .child(fetch_button("fetch-next", "Fetch next page", false))
                    .child(fetch_button("fetch-all", "Fetch all", true)),
            )
    }

    fn render_update_counts(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let mut lines: Vec<(String, bool)> = self
            .results
            .update_counts()
            .iter()
            .map(|u| (u.text(), false))
            .collect();
        for (statement, status) in self.results.statements() {
            match status {
                StatementStatus::Failed(message) => {
                    let (message, _) = display_line(message, 2000);
                    lines.push((
                        format!("Statement {}: error: {message}", statement + 1),
                        true,
                    ));
                }
                StatementStatus::Cancelled => {
                    lines.push((format!("Statement {}: cancelled", statement + 1), true));
                }
                _ => {}
            }
        }
        for (statement, t) in self.results.other_truncations() {
            lines.push((
                format!(
                    "Statement {}: truncated, memory budget reached ({} rows)",
                    statement + 1,
                    t.rows_delivered
                ),
                true,
            ));
        }
        v_flex()
            .id("update-counts")
            .size_full()
            .p_2()
            .gap_1()
            .overflow_y_scroll()
            .when(lines.is_empty(), |d| {
                d.text_color(theme.muted_foreground)
                    .child("No update counts.")
            })
            .children(lines.into_iter().map(|(text, problem)| {
                div()
                    .when(problem, |d| d.text_color(theme.danger))
                    .child(text)
            }))
    }

    fn render_notices(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let count = self.results.notices().len();
        let dropped = self.results.notices_dropped();
        v_flex()
            .size_full()
            .p_2()
            .when(dropped > 0, |d| {
                d.child(
                    div()
                        .text_color(theme.warning)
                        .child(format!("{dropped} more notices were not kept.")),
                )
            })
            .when(count == 0, |d| {
                d.text_color(theme.muted_foreground).child("No notices.")
            })
            .when(count > 0, |d| {
                // Virtualized: only the visible notices are built.
                d.child(
                    uniform_list(
                        "notices",
                        count,
                        cx.processor(|this, range: Range<usize>, _window, cx| {
                            let theme = cx.theme().clone();
                            range
                                .filter_map(|i| {
                                    let n = this.results.notices().get(i)?;
                                    let severity = match n.severity {
                                        NoticeSeverity::Warning => "WARNING",
                                        NoticeSeverity::Info => "INFO",
                                        _ => "DEBUG",
                                    };
                                    let code = n
                                        .code
                                        .as_deref()
                                        .map(|c| format!(" [{c}]"))
                                        .unwrap_or_default();
                                    let (message, _) = display_line(&n.message, 2000);
                                    Some(
                                        div()
                                            .id(("notice", i))
                                            .when(n.severity == NoticeSeverity::Warning, |d| {
                                                d.text_color(theme.warning)
                                            })
                                            .child(format!(
                                                "{}{severity}{code}: {message}",
                                                if this.results.multi_statement() {
                                                    format!("Statement {} · ", n.statement + 1)
                                                } else {
                                                    String::new()
                                                }
                                            )),
                                    )
                                })
                                .collect()
                        }),
                    )
                    .flex_1(),
                )
            })
    }
}

#[cfg(test)]
mod tests;
