//! A SQL editor tab on one table (P5-A): "Open table data" from the navigator runs the core's
//! filtered table query (`EditService::table_query`), and only such a result can be edited.
//!
//! - **Source.** [`TableSource`] holds the table's navigator path and its server-side sort and
//!   filter ([`TableQuery`], F-DATA-03). The query is built by the core (names quoted by the
//!   dialect, values bound, a raw `WHERE` checked to stay a read) and run through the execution
//!   service with its parameters: the editor shows the statement, never the values.
//! - **Editing (F-DATA-04).** Only the result of the tab's own table query is editable: a
//!   statement typed in the editor is read-only, with the explanation [`TYPED_READ_ONLY`]
//!   (`PendingEdits` matches columns by name and cannot see aliases or expressions, so only the
//!   core's query is trusted). The table's key comes from `EditService::describe_table`; a
//!   `NotEditable` answer is shown as the reason and the grid stays read-only (I8).
//! - **Saving.** `EditService::save` runs on the driver runtime; its events are awaited on the
//!   background executor and applied here (I6). A production save stops at
//!   `SaveEvent::NeedsConfirmation`: the dialog shows the exact script, and "Save changes"
//!   calls `save` again with `SaveOptions::confirmed(token)` (I3, ADR-0007). A save left in the
//!   session's open transaction (manual commit) reminds the user to commit.
//! - **Foreign keys (F-DATA-07).** The rows a row's foreign key points to, and the rows
//!   pointing to it, are opened in a new tab through `referenced_rows_query` /
//!   `referencing_rows_query`, whose results are editable again (they are table queries).
//! - Every action is refused while the session is leased by a save or a transfer, while a run
//!   is going on, or while a save waits for its confirmation.

use std::collections::{BTreeMap, BTreeSet};

use gpui_kit::*;
use ninesql_core::driver_api::arrow_array::RecordBatch;
use ninesql_core::driver_api::metadata::{ForeignKeyInfo, ObjectDetails, ObjectPath};
use ninesql_core::driver_api::{ObjectKind, Param};
use ninesql_core::edit::{
    referenced_rows_query, referencing_rows_query, row_values_by_name, table_parts,
};
use ninesql_core::metadata::NodeKey;
use ninesql_core::{
    BuiltQuery, ColumnFilter, ColumnStatus, EditError, EditableTable, FilterHistoryEntry, FilterOp,
    FilterRecorded, NewRowId, PendingEdits, SaveConfirmation, SaveEvent, SaveOptions, SavePlan,
    SaveSummary, SaveToken, SessionHandle, SortDirection, SortKey, TableQuery,
};
use ninesql_editor::execution::RequestId;
use ninesql_grid::model::ResultSetModel;
use ninesql_grid::{EditBuffer, EditValue, Editability};

use super::SqlEditorTab;

/// Why a result typed in the editor cannot be edited.
pub const TYPED_READ_ONLY: &str =
    "Read-only: open the table from the navigator (Open table data) to edit its rows.";

/// The table a tab shows, with its server-side sort and filter.
#[derive(Clone)]
pub struct TableSource {
    /// Navigator path of the table.
    pub path: ObjectPath,
    /// The current query (filters, raw `WHERE`, order).
    pub query: TableQuery,
    /// A query to run first instead of [`Self::query`] (foreign-key navigation).
    pub initial: Option<BuiltQuery>,
}

impl std::fmt::Debug for TableSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TableSource")
            .field("query", &self.query)
            .finish_non_exhaustive()
    }
}

impl TableSource {
    /// Every row of the table at `path`.
    #[must_use]
    pub fn of_path(path: ObjectPath) -> Self {
        Self {
            query: TableQuery::of_path(&path),
            path,
            initial: None,
        }
    }

    /// The table's display name (`schema.table`).
    #[must_use]
    pub fn label(&self) -> String {
        table_parts(&self.path).join(".")
    }
}

/// The pending changes of the tab's editable result.
pub struct Editing {
    /// The table and its key.
    pub table: EditableTable,
    /// Grid result tab index.
    pub tab: usize,
    /// The core's pending changes (the truth).
    pub pending: PendingEdits,
    /// What the user typed per cell, for the grid's pending values.
    typed: BTreeMap<(u64, usize), EditValue>,
    deleted: BTreeSet<u64>,
}

/// Whether the tab's result can be edited.
pub enum EditMode {
    /// Read-only, with the reason shown.
    ReadOnly(String),
    /// The table's key is being read.
    Describing,
    /// Editable.
    Editable(Box<Editing>),
}

/// A foreign-key target offered by the navigation actions.
#[derive(Clone)]
pub struct FkTarget {
    /// Label ("customers (customer_id)").
    pub label: String,
    /// The table to open.
    pub path: ObjectPath,
    /// Its bound query.
    pub query: BuiltQuery,
}

impl std::fmt::Debug for FkTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FkTarget")
            .field("label", &self.label)
            .finish_non_exhaustive()
    }
}

/// Table state of a tab (owned by [`SqlEditorTab`]).
pub struct TableState {
    /// The tab's table, when opened from the navigator.
    pub source: Option<TableSource>,
    /// Request id of the table query submitted to the editor.
    pub source_request: Option<RequestId>,
    /// The current run (or the last one) is the table query.
    pub source_live: bool,
    /// The table's description, once read.
    pub described: Option<Result<EditableTable, String>>,
    /// Edit state.
    pub mode: EditMode,
    /// A save waiting for the user's confirmation.
    pub confirmation: Option<SaveConfirmation>,
    /// A save is running.
    pub saving: bool,
    pub(super) describing: Option<Task<()>>,
    pub(super) save_task: Option<Task<()>>,
}

impl Default for TableState {
    fn default() -> Self {
        Self {
            source: None,
            source_request: None,
            source_live: false,
            described: None,
            mode: EditMode::ReadOnly(TYPED_READ_ONLY.to_owned()),
            confirmation: None,
            saving: false,
            describing: None,
            save_task: None,
        }
    }
}

/// Builds the pending changes of a result of `source` (one place, so the core's provenance
/// check plugs in here).
fn pending_for(
    table: EditableTable,
    model: &ResultSetModel,
    _source: &TableSource,
) -> Result<PendingEdits, String> {
    PendingEdits::new(table, model.schema()).map_err(|e| format!("Read-only: {e}."))
}

/// The batch and in-batch row of model row `row`.
fn batch_row(model: &ResultSetModel, row: usize) -> Option<(RecordBatch, usize)> {
    let (b, r) = model.locate(row)?;
    Some((model.batches().get(b)?.clone(), r))
}

fn edit_text(e: &EditError) -> String {
    crate::browse::display_message(&e.to_string())
}

/// The input text of a cell for the edit dialog: the pending value typed, else the loaded one.
fn cell_input(model: &ResultSetModel, row: usize, col: usize) -> Option<String> {
    let mut out = String::new();
    model.write_cell_copy(&mut out, row, col, "");
    Some(out)
}

impl SqlEditorTab {
    /// The tab's table state.
    #[must_use]
    pub fn table_state(&self) -> &TableState {
        &self.table
    }

    /// Why the result cannot be edited, if so.
    #[must_use]
    pub fn read_only_reason(&self) -> Option<&str> {
        match &self.table.mode {
            EditMode::ReadOnly(why) => Some(why),
            EditMode::Describing => Some("Reading the table's key…"),
            EditMode::Editable(_) => None,
        }
    }

    /// The pending changes, when the result is editable.
    #[must_use]
    pub fn editing(&self) -> Option<&Editing> {
        match &self.table.mode {
            EditMode::Editable(e) => Some(e),
            _ => None,
        }
    }

    /// Whether edits are pending.
    #[must_use]
    pub fn has_pending_edits(&self) -> bool {
        self.editing().is_some_and(|e| !e.pending.is_empty())
    }

    /// Why a table action cannot run now, if so (I8).
    fn table_action_problem(&self) -> Option<&'static str> {
        let Some(session) = self.session() else {
            return Some("No open connection for this editor.");
        };
        if session.is_leased() {
            Some("A save, export or import is using this session: wait for it to end.")
        } else if self.is_running() {
            Some("Wait for the run to end.")
        } else if self.table.saving {
            Some("A save is running: wait for it to end.")
        } else if self.table.confirmation.is_some() {
            Some("A save waits for its confirmation.")
        } else {
            None
        }
    }

    // --- source ------------------------------------------------------------------------------

    /// Opens `source` in this tab: reads the table's key and runs its query.
    pub fn open_table(&mut self, source: TableSource, window: &mut Window, cx: &mut Context<Self>) {
        self.title = source.label().into();
        self.table.source = Some(source);
        self.table.described = None;
        self.describe_source(cx);
        self.run_source(window, cx);
    }

    /// Reads the key of the source table (`EditService::describe_table`).
    fn describe_source(&mut self, cx: &mut Context<Self>) {
        let (Some(source), Some(session)) = (self.table.source.clone(), self.session().cloned())
        else {
            return;
        };
        let Some(edit) = self.hub.update(cx, |h, _| h.edit()) else {
            self.table.described = Some(Err(
                "Read-only: the driver runtime is not running.".to_owned()
            ));
            return;
        };
        let reply = edit.describe_table(&session, source.path.clone());
        self.table.mode = EditMode::Describing;
        self.table.describing = Some(cx.spawn(async move |this, cx| {
            let result = reply.await;
            let _ = this.update(cx, |tab, cx| {
                let described = match result {
                    Some(Ok(t)) => Ok(t),
                    Some(Err(e)) => Err(format!("Read-only: {}.", edit_text(&e))),
                    None => Err("Read-only: the session closed.".to_owned()),
                };
                tab.set_description(described, cx);
            });
        }));
    }

    /// Records the table's description (from `describe_table`; tests give their own).
    pub fn set_description(
        &mut self,
        described: Result<EditableTable, String>,
        cx: &mut Context<Self>,
    ) {
        self.table.describing = None;
        self.table.described = Some(described);
        self.refresh_editing(cx);
        cx.notify();
    }

    /// Runs the source's query (the initial one first): the editor shows the statement and the
    /// core gets its parameters. Refused with pending edits.
    pub fn run_source(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if let Some(why) = self.table_action_problem() {
            self.say(why, true, cx);
            return false;
        }
        if self.has_pending_edits() {
            self.say(
                "There are unsaved changes: save or discard them first (Save changes / Discard changes).",
                true,
                cx,
            );
            return false;
        }
        let Some(session) = self.session().cloned() else {
            return false;
        };
        let Some(source) = self.table.source.as_mut() else {
            self.say(
                "This editor does not show a table: open one from the navigator (Open table data).",
                true,
                cx,
            );
            return false;
        };
        let built = match source.initial.take() {
            Some(b) => b,
            None => {
                let Some(edit) = self.hub.update(cx, |h, _| h.edit()) else {
                    self.say("The driver runtime is not running.", true, cx);
                    return false;
                };
                let q = self.table.source.as_ref().map(|s| s.query.clone());
                match q.map(|q| edit.table_query(&session, &q)) {
                    Some(Ok(b)) => b,
                    Some(Err(e)) => {
                        let text = crate::browse::display_message(&e.to_string());
                        self.say(text, true, cx);
                        return false;
                    }
                    None => return false,
                }
            }
        };
        let BuiltQuery { text, params, .. } = built;
        let submitted = self
            .editor
            .update(cx, |e, cx| e.submit_built(&text, params, window, cx));
        match submitted {
            Ok(id) => {
                self.table.source_request = Some(id);
                true
            }
            Err(why) => {
                self.say(why, true, cx);
                false
            }
        }
    }

    /// Called when a run starts: a run of the table query keeps editing possible, any other
    /// run makes the result read-only.
    pub(super) fn table_run_started(&mut self, request: RequestId) {
        self.table.source_live = self.table.source_request == Some(request);
        if self.table.source_live {
            self.table.mode = match &self.table.described {
                None => EditMode::Describing,
                Some(Err(why)) => EditMode::ReadOnly(why.clone()),
                Some(Ok(_)) => EditMode::Describing,
            };
        } else {
            self.table.mode = EditMode::ReadOnly(TYPED_READ_ONLY.to_owned());
        }
    }

    /// Makes the result editable once both the table's description and the result's header
    /// are there (statement 0 of the table query).
    pub(super) fn refresh_editing(&mut self, cx: &mut Context<Self>) {
        if !self.table.source_live || matches!(self.table.mode, EditMode::Editable(_)) {
            return;
        }
        let Some(described) = self.table.described.clone() else {
            return;
        };
        let table = match described {
            Ok(t) => t,
            Err(why) => {
                self.table.mode = EditMode::ReadOnly(why);
                return;
            }
        };
        let Some(source) = self.table.source.clone() else {
            return;
        };
        let results = self.results.read(cx);
        let Some(ix) = results.tab_of_statement(0) else {
            return;
        };
        let Some(ninesql_grid::ResultTab::Table(model)) = results.results().tabs().get(ix) else {
            return;
        };
        let model = model.clone();
        let built = pending_for(table.clone(), &model.borrow(), &source);
        self.table.mode = match built {
            Ok(pending) => EditMode::Editable(Box::new(Editing {
                table,
                tab: ix,
                pending,
                typed: BTreeMap::new(),
                deleted: BTreeSet::new(),
            })),
            Err(why) => EditMode::ReadOnly(why),
        };
        self.sync_grid(cx);
    }

    /// Mirrors the pending changes into the grid (dirty markers, pending values, deleted rows).
    fn sync_grid(&mut self, cx: &mut Context<Self>) {
        let caps = self
            .session()
            .map(|s| s.info().capabilities.editing)
            .unwrap_or_default();
        let EditMode::Editable(e) = &mut self.table.mode else {
            return;
        };
        let ix = e.tab;
        let Some(ninesql_grid::ResultTab::Table(model)) =
            self.results.read(cx).results().tabs().get(ix).cloned()
        else {
            return;
        };
        let model = model.borrow();
        let n = model.columns().len();
        let columns: Vec<usize> = (0..n)
            .filter(|f| e.pending.column_status(*f) == Some(ColumnStatus::Editable))
            .filter(|_| caps.update)
            .collect();
        let key: Vec<usize> = (0..n).filter(|f| e.pending.is_key_column(*f)).collect();
        let mut buffer = EditBuffer::new();
        buffer.set_editability(Some(Editability::Editable {
            columns,
            key,
            insert: caps.insert,
            delete: caps.delete,
        }));
        e.typed
            .retain(|(row, col), _| e.pending.changed_value(*row, *col).is_some());
        e.deleted.retain(|row| e.pending.is_deleted(*row));
        for ((row, col), value) in &e.typed {
            let _ = buffer.set_cell(
                &model,
                usize::try_from(*row).unwrap_or(usize::MAX),
                *col,
                value.clone(),
            );
        }
        for row in &e.deleted {
            let _ = buffer.delete_row(&model, usize::try_from(*row).unwrap_or(usize::MAX));
        }
        for _ in e.pending.new_rows() {
            let _ = buffer.insert_row(&model);
        }
        drop(model);
        self.results
            .update(cx, |r, cx| r.set_edit_buffer(ix, Some(buffer), cx));
        cx.notify();
    }

    /// The editable state and the model of its result, or the reason shown.
    fn editable_model(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<std::rc::Rc<std::cell::RefCell<ResultSetModel>>> {
        if let Some(why) = self.table_action_problem() {
            self.say(why, true, cx);
            return None;
        }
        let ix = match &self.table.mode {
            EditMode::Editable(e) => e.tab,
            EditMode::ReadOnly(why) => {
                let why = why.clone();
                self.say(why, true, cx);
                return None;
            }
            EditMode::Describing => {
                self.say("Reading the table's key: try again in a moment.", false, cx);
                return None;
            }
        };
        match self.results.read(cx).results().tabs().get(ix) {
            Some(ninesql_grid::ResultTab::Table(m)) => Some(m.clone()),
            _ => None,
        }
    }

    /// The focused cell of the editable result, as (model row, model column).
    fn focused_edit_cell(&mut self, cx: &mut Context<Self>) -> Option<(usize, usize)> {
        let ix = self.editing()?.tab;
        match self.results.read(cx).focused_cell(cx) {
            Some((t, r, c)) if t == ix => Some((r, c)),
            _ => {
                self.say("Select a cell of the table's result first.", true, cx);
                None
            }
        }
    }

    fn with_editing<T>(&mut self, f: impl FnOnce(&mut Editing) -> T) -> Option<T> {
        match &mut self.table.mode {
            EditMode::Editable(e) => Some(f(e)),
            _ => None,
        }
    }

    // --- editing -----------------------------------------------------------------------------

    /// What the cell edit dialog needs for the focused cell: (row, column, name, type, text),
    /// or `None` with the reason shown.
    pub fn cell_to_edit(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<(usize, usize, String, String)> {
        let model = self.editable_model(cx)?;
        if !self
            .session()
            .is_some_and(|s| s.info().capabilities.editing.update)
        {
            self.say("This engine does not support changing rows.", true, cx);
            return None;
        }
        let (row, col) = self.focused_edit_cell(cx)?;
        let editing = self.editing()?;
        if editing.pending.column_status(col) != Some(ColumnStatus::Editable) {
            self.say(
                "This column is not a column of the table (or appears twice): it cannot be edited.",
                true,
                cx,
            );
            return None;
        }
        let m = model.borrow();
        let name = m
            .columns()
            .get(col)
            .map(|c| c.name.clone())
            .unwrap_or_default();
        let text = match editing.typed.get(&(row as u64, col)) {
            Some(EditValue::Text(t)) => t.clone(),
            Some(_) => String::new(),
            None => cell_input(&m, row, col).unwrap_or_default(),
        };
        Some((row, col, name, text))
    }

    /// Sets a cell of a loaded row: `None` sets `NULL`, text is converted to the column's type
    /// by the core. Returns the error text when refused.
    pub fn set_cell(
        &mut self,
        row: usize,
        col: usize,
        text: Option<&str>,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let Some(model) = self.editable_model(cx) else {
            return Err(self
                .message
                .as_ref()
                .map_or_else(String::new, |m| m.text.to_string()));
        };
        let Some((batch, batch_row)) = batch_row(&model.borrow(), row) else {
            return Err("The row is no longer loaded.".to_owned());
        };
        let result = self
            .with_editing(|e| {
                let value = match text {
                    None => Param::Null,
                    Some(t) => e.pending.parse_input(col, t)?,
                };
                e.pending
                    .set_value(row as u64, &batch, batch_row, col, value)?;
                e.typed.insert(
                    (row as u64, col),
                    text.map_or(EditValue::Null, |t| EditValue::Text(t.to_owned())),
                );
                Ok::<(), EditError>(())
            })
            .unwrap_or(Ok(()));
        match result {
            Ok(()) => {
                self.sync_grid(cx);
                Ok(())
            }
            Err(e) => {
                let text = edit_text(&e);
                self.say(text.clone(), true, cx);
                Err(text)
            }
        }
    }

    /// "Set cell to NULL" on the focused cell.
    pub fn set_focused_null(&mut self, cx: &mut Context<Self>) {
        if let Some((row, col, _, _)) = self.cell_to_edit(cx) {
            let _ = self.set_cell(row, col, None, cx);
        }
    }

    /// "Delete rows": marks the selected rows (the focused one for a cell) for deletion.
    pub fn delete_selected_rows(&mut self, cx: &mut Context<Self>) {
        let Some(model) = self.editable_model(cx) else {
            return;
        };
        if !self
            .session()
            .is_some_and(|s| s.info().capabilities.editing.delete)
        {
            return self.say("This engine does not support deleting rows.", true, cx);
        }
        let Some(ix) = self.editing().map(|e| e.tab) else {
            return;
        };
        let rows = match self.results.read(cx).selected_model_rows(cx) {
            Some((t, rows)) if t == ix => rows,
            _ => return self.say("Select rows of the table's result first.", true, cx),
        };
        let m = model.borrow();
        let mut error = None;
        for row in rows {
            let Some((batch, batch_row)) = batch_row(&m, row) else {
                continue;
            };
            let r = self.with_editing(|e| {
                e.pending.delete_row(row as u64, &batch, batch_row)?;
                e.deleted.insert(row as u64);
                Ok::<(), EditError>(())
            });
            if let Some(Err(e)) = r {
                error = Some(edit_text(&e));
                break;
            }
        }
        drop(m);
        if let Some(e) = error {
            self.say(e, true, cx);
        }
        self.sync_grid(cx);
    }

    /// "Revert row changes": the selected rows' edits and deletions are dropped.
    pub fn revert_selected_rows(&mut self, cx: &mut Context<Self>) {
        let Some(ix) = self.editing().map(|e| e.tab) else {
            return self.say(
                "Nothing to revert: the result is not being edited.",
                false,
                cx,
            );
        };
        let rows = match self.results.read(cx).selected_model_rows(cx) {
            Some((t, rows)) if t == ix => rows,
            _ => return self.say("Select rows of the table's result first.", true, cx),
        };
        self.with_editing(|e| {
            for row in rows {
                e.pending.revert_row(row as u64);
            }
        });
        self.sync_grid(cx);
    }

    /// "Insert row": adds a row with the given values (`None`: the column's default; `Some(None)`
    /// would be NULL, given as `Some(None)` text-less). Returns the new row or the error text.
    pub fn insert_row_with(
        &mut self,
        values: Vec<(usize, Option<String>)>,
        cx: &mut Context<Self>,
    ) -> Result<NewRowId, String> {
        if self.editable_model(cx).is_none() {
            return Err("Not editable.".to_owned());
        }
        if !self
            .session()
            .is_some_and(|s| s.info().capabilities.editing.insert)
        {
            let why = "This engine does not support inserting rows.";
            self.say(why, true, cx);
            return Err(why.to_owned());
        }
        let result = self
            .with_editing(|e| {
                let row = e.pending.insert_row();
                for (field, text) in values {
                    let value = match text {
                        None => Param::Null,
                        Some(t) => match e.pending.parse_input(field, &t) {
                            Ok(v) => v,
                            Err(err) => {
                                e.pending.remove_new_row(row);
                                return Err(err);
                            }
                        },
                    };
                    if let Err(err) = e.pending.set_new_value(row, field, Some(value)) {
                        e.pending.remove_new_row(row);
                        return Err(err);
                    }
                }
                Ok(row)
            })
            .unwrap_or(Err(EditError::NoSuchRow));
        match result {
            Ok(row) => {
                self.sync_grid(cx);
                Ok(row)
            }
            Err(e) => {
                let text = edit_text(&e);
                self.say(text.clone(), true, cx);
                Err(text)
            }
        }
    }

    /// The editable columns of the result for the "Insert row" form: (field, name, type).
    #[must_use]
    pub fn insert_columns(&self, cx: &App) -> Vec<(usize, String, String)> {
        let Some(e) = self.editing() else {
            return Vec::new();
        };
        let Some(ninesql_grid::ResultTab::Table(model)) =
            self.results.read(cx).results().tabs().get(e.tab)
        else {
            return Vec::new();
        };
        let m = model.borrow();
        m.columns()
            .iter()
            .enumerate()
            .filter(|(i, _)| e.pending.column_status(*i) == Some(ColumnStatus::Editable))
            .map(|(i, c)| {
                let ty = e
                    .table
                    .column(&c.name)
                    .map_or_else(|| c.type_name().to_owned(), |t| t.data_type.clone());
                (i, c.name.clone(), ty)
            })
            .collect()
    }

    /// "Duplicate row": adds a copy of the focused row (engine-generated columns get their
    /// default).
    pub fn duplicate_focused_row(&mut self, cx: &mut Context<Self>) {
        let Some(model) = self.editable_model(cx) else {
            return;
        };
        if !self
            .session()
            .is_some_and(|s| s.info().capabilities.editing.insert)
        {
            return self.say("This engine does not support inserting rows.", true, cx);
        }
        let Some((row, _)) = self.focused_edit_cell(cx) else {
            return;
        };
        let Some((batch, batch_row)) = batch_row(&model.borrow(), row) else {
            return;
        };
        match self.with_editing(|e| e.pending.duplicate_row(&batch, batch_row)) {
            Some(Ok(_)) => {
                self.say("Row duplicated: save to insert it.", false, cx);
                self.sync_grid(cx);
            }
            Some(Err(e)) => self.say(edit_text(&e), true, cx),
            None => {}
        }
    }

    /// "Discard changes": every pending change is dropped.
    pub fn discard_changes(&mut self, cx: &mut Context<Self>) {
        if self.table.saving || self.table.confirmation.is_some() {
            return self.say("A save is going on: wait for it to end.", true, cx);
        }
        if self.with_editing(|e| e.pending.clear()).is_some() {
            self.say("Changes discarded.", false, cx);
            self.sync_grid(cx);
        }
    }

    /// The DML preview of the pending changes ("Preview changes").
    ///
    /// # Errors
    /// The text shown (nothing to save, a change the generator refuses).
    pub fn plan_changes(&mut self, cx: &mut Context<Self>) -> Result<SavePlan, String> {
        let Some(session) = self.session().cloned() else {
            return Err("No open connection for this editor.".to_owned());
        };
        let Some(e) = self.editing() else {
            return Err(self
                .read_only_reason()
                .unwrap_or("Not editable.")
                .to_owned());
        };
        let plan = e
            .pending
            .plan(&*session.dialect(), &session.info().capabilities.editing)
            .map_err(|e| edit_text(&e));
        if let Err(why) = &plan {
            self.say(why.clone(), true, cx);
        }
        plan
    }

    /// "Save changes": saves through the core; a production save asks first
    /// ([`Self::confirmation`]).
    pub fn save_changes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.start_save(None, window, cx);
    }

    /// The save waiting for confirmation (its exact script), if any.
    #[must_use]
    pub fn confirmation(&self) -> Option<&SaveConfirmation> {
        self.table.confirmation.as_ref()
    }

    /// "Save changes" in the confirmation: saves again with the token.
    pub fn confirm_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(c) = self.table.confirmation.take() else {
            return;
        };
        self.start_save(Some(c.token), window, cx);
    }

    /// "Cancel" in the confirmation: nothing is saved, the changes stay pending.
    pub fn decline_save(&mut self, cx: &mut Context<Self>) {
        if self.table.confirmation.take().is_some() {
            self.say(
                "Not saved: the changes were not confirmed. They are still pending.",
                false,
                cx,
            );
        }
    }

    fn start_save(
        &mut self,
        token: Option<SaveToken>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.table.saving || (token.is_none() && self.table.confirmation.is_some()) {
            return self.say("A save is going on: wait for it to end.", true, cx);
        }
        if self.is_running() {
            return self.say("Wait for the run to end before saving.", true, cx);
        }
        let Some(session) = self.session().cloned() else {
            return self.say("No open connection for this editor.", true, cx);
        };
        if session.is_leased() {
            return self.say(
                "An export or import is using this session: wait for it to end.",
                true,
                cx,
            );
        }
        let Some(edit) = self.hub.update(cx, |h, _| h.edit()) else {
            return self.say("The driver runtime is not running.", true, cx);
        };
        let Some(e) = self.editing() else {
            let why = self
                .read_only_reason()
                .unwrap_or("Not editable.")
                .to_owned();
            return self.say(why, true, cx);
        };
        if e.pending.is_empty() {
            return self.say("There are no changes to save.", false, cx);
        }
        let mut options = SaveOptions::default();
        if let Some(t) = token {
            options = options.confirmed(t);
        }
        let job = edit.save(&session, &e.pending, options);
        self.table.saving = true;
        self.say("Saving…", false, cx);
        self.table.save_task = Some(cx.spawn_in(window, async move |this, cx| {
            let mut job = job;
            loop {
                let (back, event) = cx
                    .background_spawn(async move {
                        let event = job.next().await;
                        (job, event)
                    })
                    .await;
                job = back;
                let Some(event) = event else { break };
                let terminal = matches!(
                    event,
                    SaveEvent::Done(_) | SaveEvent::Failed(_) | SaveEvent::NeedsConfirmation(_)
                );
                let applied =
                    this.update_in(cx, |tab, window, cx| tab.on_save_event(event, window, cx));
                if applied.is_err() || terminal {
                    break;
                }
            }
            let _ = this.update(cx, |tab, cx| {
                if tab.table.saving {
                    tab.table.saving = false;
                    tab.say(
                        "The save ended without an answer: nothing was saved.",
                        true,
                        cx,
                    );
                }
            });
        }));
        cx.notify();
    }

    fn on_save_event(&mut self, event: SaveEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            SaveEvent::NeedsConfirmation(c) => {
                self.table.saving = false;
                self.table.confirmation = Some(c.clone());
                self.message = None;
                super::edit_dialogs::open_save_confirmation(cx.entity(), c, window, cx);
            }
            SaveEvent::Started { statements } => {
                self.say(format!("Saving {statements} changes…"), false, cx);
            }
            SaveEvent::Progress { done, total } => {
                self.say(format!("Saving: {done} of {total}…"), false, cx);
            }
            SaveEvent::Done(summary) => {
                self.table.saving = false;
                self.with_editing(|e| e.pending.clear());
                self.sync_grid(cx);
                self.say(save_done_text(&summary), !summary.committed, cx);
                // Show the rows as saved (the same session sees its own open transaction).
                if self.table.source.is_some() {
                    let message = self.message.clone();
                    self.run_source(window, cx);
                    self.message = message;
                }
            }
            SaveEvent::Failed(e) => {
                self.table.saving = false;
                self.say(
                    format!(
                        "Not saved: {} The changes are still pending.",
                        edit_text(&e)
                    ),
                    true,
                    cx,
                );
            }
            _ => {}
        }
        cx.notify();
    }

    // --- server-side sort and filter ----------------------------------------------------------

    /// The focused cell's column name and value in any result tab of the tab's table query.
    fn focused_source_cell(&mut self, cx: &mut Context<Self>) -> Option<(String, Param)> {
        if self.table.source.is_none() || !self.table.source_live {
            self.say(
                "Server-side sort and filter apply to a table opened from the navigator (Open table data).",
                true,
                cx,
            );
            return None;
        }
        let results = self.results.read(cx);
        let Some((ix, row, col)) = results.focused_cell(cx) else {
            self.say("Select a cell first.", true, cx);
            return None;
        };
        let Some(ninesql_grid::ResultTab::Table(model)) = results.results().tabs().get(ix) else {
            return None;
        };
        let m = model.borrow();
        let name = m.columns().get(col)?.name.clone();
        let (batch, batch_row) = batch_row(&m, row)?;
        drop(m);
        match row_values_by_name(&batch, batch_row, std::slice::from_ref(&name)) {
            Ok(mut v) => Some((name, v.pop().unwrap_or(Param::Null))),
            Err(e) => {
                self.say(edit_text(&e), true, cx);
                None
            }
        }
    }

    /// "Sort table by column (server)".
    pub fn sort_server(&mut self, descending: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some((column, _)) = self.focused_source_cell(cx) else {
            return;
        };
        let direction = if descending {
            SortDirection::Descending
        } else {
            SortDirection::Ascending
        };
        let previous = self.table.source.as_ref().map(|s| s.query.clone());
        if let Some(s) = self.table.source.as_mut() {
            s.query.order_by = vec![SortKey::new(column, direction)];
        }
        self.apply_query(previous, window, cx);
    }

    /// "Filter table by selected value (server)": adds `column = value` (or `IS NULL`).
    pub fn filter_server_by_value(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((column, value)) = self.focused_source_cell(cx) else {
            return;
        };
        let filter = if value == Param::Null {
            ColumnFilter::new(column, FilterOp::IsNull, None)
        } else {
            ColumnFilter::new(column, FilterOp::Equal, Some(value))
        };
        let previous = self.table.source.as_ref().map(|s| s.query.clone());
        if let Some(s) = self.table.source.as_mut() {
            s.query.filters.push(filter);
        }
        self.apply_query(previous, window, cx);
    }

    /// Sets the raw `WHERE` of the table query (`None` or blank: none) and runs it.
    ///
    /// # Errors
    /// The core's `QueryError` text, or why it cannot run now; the query is left unchanged.
    pub fn set_raw_where(
        &mut self,
        raw: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let Some(session) = self.session().cloned() else {
            return Err("No open connection for this editor.".to_owned());
        };
        let Some(source) = self.table.source.as_ref() else {
            return Err("This editor does not show a table.".to_owned());
        };
        let mut q = source.query.clone();
        q.raw_where = raw.filter(|r| !r.trim().is_empty());
        let edit = self
            .hub
            .update(cx, |h, _| h.edit())
            .ok_or("The driver runtime is not running.")?;
        edit.table_query(&session, &q)
            .map_err(|e| crate::browse::display_message(&e.to_string()))?;
        let previous = Some(source.query.clone());
        if let Some(s) = self.table.source.as_mut() {
            s.query = q;
        }
        if self.apply_query(previous, window, cx) {
            Ok(())
        } else {
            Err(self
                .message
                .as_ref()
                .map_or_else(|| "Not run.".to_owned(), |m| m.text.to_string()))
        }
    }

    /// Applies a filter from the history.
    pub fn apply_history_entry(
        &mut self,
        entry: &FilterHistoryEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let previous = self.table.source.as_ref().map(|s| s.query.clone());
        let Some(s) = self.table.source.as_mut() else {
            return false;
        };
        if entry.table != s.query.table {
            return false;
        }
        s.query = entry.to_query();
        let partial = !entry.complete;
        let ran = self.apply_query(previous, window, cx);
        if ran && partial {
            self.say(
                "Some values of this filter were not kept in the history: they were left out.",
                false,
                cx,
            );
        }
        ran
    }

    /// "Clear table filter (server)": every filter and the order are removed.
    pub fn clear_server_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let previous = self.table.source.as_ref().map(|s| s.query.clone());
        if let Some(s) = self.table.source.as_mut() {
            s.query = TableQuery::of_path(&s.path);
        } else {
            return self.say("This editor does not show a table.", true, cx);
        }
        self.apply_query(previous, window, cx);
    }

    /// Runs the source's new query and records it in the filter history; puts `previous` back
    /// when it cannot run.
    fn apply_query(
        &mut self,
        previous: Option<TableQuery>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.run_source(window, cx) {
            if let (Some(session), Some(edit), Some(q)) = (
                self.session().cloned(),
                self.hub.update(cx, |h, _| h.edit()),
                self.table.source.as_ref().map(|s| s.query.clone()),
            ) && let FilterRecorded::Partial = edit.record_filter(&session, &q)
            {
                tracing::debug!("filter recorded without some values");
            }
            true
        } else {
            if let (Some(p), Some(s)) = (previous, self.table.source.as_mut()) {
                s.query = p;
            }
            false
        }
    }

    /// The filter history of the tab's table, newest first.
    #[must_use]
    pub fn filter_history(&mut self, cx: &mut Context<Self>) -> Vec<FilterHistoryEntry> {
        let (Some(connection), Some(table)) = (
            self.connection(),
            self.table.source.as_ref().map(|s| s.query.table.clone()),
        ) else {
            return Vec::new();
        };
        self.hub
            .update(cx, |h, _| h.edit())
            .map(|e| e.filter_history(connection, Some(&table)))
            .unwrap_or_default()
    }

    // --- foreign keys -------------------------------------------------------------------------

    /// The targets of "Open referenced rows" (`referencing = false`: the rows the focused row's
    /// foreign keys point to) or "Open referencing rows" (the rows pointing to it). The focused
    /// column's keys come first.
    pub fn foreign_key_targets(
        &mut self,
        referencing: bool,
        cx: &mut Context<Self>,
    ) -> Task<Result<Vec<FkTarget>, String>> {
        let fail = |this: &mut Self, why: &str, cx: &mut Context<Self>| {
            this.say(why.to_owned(), true, cx);
            Task::ready(Err(why.to_owned()))
        };
        let (Some(source), true) = (self.table.source.clone(), self.table.source_live) else {
            return fail(
                self,
                "Foreign-key navigation needs a table opened from the navigator (Open table data).",
                cx,
            );
        };
        let Some(session) = self.session().cloned() else {
            return fail(self, "No open connection for this editor.", cx);
        };
        let picked = (|| {
            let results = self.results.read(cx);
            let (ix, row, col) = results
                .focused_cell(cx)
                .ok_or("Select a cell of a row first.")?;
            let Some(ninesql_grid::ResultTab::Table(model)) = results.results().tabs().get(ix)
            else {
                return Err("Select a cell of a row first.");
            };
            let m = model.borrow();
            let column = m.columns().get(col).map(|c| c.name.clone());
            let (batch, batch_row) = batch_row(&m, row).ok_or("The row is no longer loaded.")?;
            Ok((column, batch, batch_row))
        })();
        let (column, batch, batch_row) = match picked {
            Ok(p) => p,
            Err(why) => return fail(self, why, cx),
        };
        let dialect = session.dialect();
        let connection = session.info().connection;
        if referencing {
            let Some(edit) = self.hub.update(cx, |h, _| h.edit()) else {
                return fail(self, "The driver runtime is not running.", cx);
            };
            let reply = edit.referencing_foreign_keys(&session, source.path.clone());
            return cx.spawn(async move |this, cx| {
                let keys = match reply.await {
                    Some(Ok(k)) => k,
                    Some(Err(e)) => return Err(edit_text(&e)),
                    None => return Err("The session closed.".to_owned()),
                };
                let mut out = Vec::new();
                let mut last_error = None;
                for (table, fk) in &keys.keys {
                    let values = match row_values_by_name(&batch, batch_row, &fk.referenced_columns)
                    {
                        Ok(v) => v,
                        Err(e) => {
                            last_error = Some(edit_text(&e));
                            continue;
                        }
                    };
                    match referencing_rows_query(&*dialect, table, fk, &values) {
                        Ok(query) => out.push(FkTarget {
                            label: format!(
                                "{} ({})",
                                table_parts(table).join("."),
                                fk.columns.join(", ")
                            ),
                            path: table.clone(),
                            query,
                        }),
                        Err(e) => last_error = Some(e.to_string()),
                    }
                }
                let complete = keys.complete;
                let _ = this.update(cx, |tab, cx| {
                    if !complete {
                        tab.say(
                            "Only the foreign keys of the same schema were searched (or the list was cut).",
                            false,
                            cx,
                        );
                    }
                });
                if out.is_empty() {
                    return Err(last_error
                        .unwrap_or_else(|| "No foreign key references this table.".to_owned()));
                }
                Ok(out)
            });
        }
        let Some(metadata) = self.hub.read(cx).metadata() else {
            return fail(self, "The driver runtime is not running.", cx);
        };
        let kinds = metadata
            .child_kinds(connection, &source.path)
            .unwrap_or_default();
        if !kinds.contains(&ObjectKind::ForeignKey) {
            return fail(self, "This engine does not list foreign keys.", cx);
        }
        let request = metadata.children(
            connection,
            NodeKey::new(source.path.clone(), ObjectKind::ForeignKey),
        );
        cx.spawn(async move |_, _| {
            let node = request.await.map_err(|e| e.to_string())?;
            let mut fks: Vec<(String, ForeignKeyInfo)> = node
                .items
                .iter()
                .filter_map(|o| match &o.details {
                    ObjectDetails::ForeignKey(fk) => Some((o.name().to_owned(), fk.clone())),
                    _ => None,
                })
                .collect();
            // The focused column's keys first.
            fks.sort_by_key(|(_, fk)| !column.as_ref().is_some_and(|c| fk.columns.contains(c)));
            let mut out = Vec::new();
            let mut last_error = None;
            for (name, fk) in &fks {
                let values = match row_values_by_name(&batch, batch_row, &fk.columns) {
                    Ok(v) => v,
                    Err(e) => {
                        last_error = Some(edit_text(&e));
                        continue;
                    }
                };
                match referenced_rows_query(&*dialect, fk, &values) {
                    Ok(query) => out.push(FkTarget {
                        label: format!(
                            "{} → {} ({})",
                            crate::browse::display_message(name),
                            table_parts(&fk.referenced_table).join("."),
                            fk.columns.join(", ")
                        ),
                        path: fk.referenced_table.clone(),
                        query,
                    }),
                    Err(e) => last_error = Some(e.to_string()),
                }
            }
            if out.is_empty() {
                return Err(
                    last_error.unwrap_or_else(|| "This table has no foreign key.".to_owned())
                );
            }
            Ok(out)
        })
    }

    /// The tab's session, for opening a foreign-key target in a new tab.
    #[must_use]
    pub fn table_session(&self) -> Option<SessionHandle> {
        self.session().cloned()
    }
}

/// The message after a save.
#[must_use]
pub fn save_done_text(summary: &SaveSummary) -> String {
    let mut text = format!("Saved {} changes", summary.statements);
    if summary.committed {
        text.push('.');
    } else {
        text.push_str(
            " in the open transaction: commit to keep them (Commit transaction), or roll back.",
        );
    }
    if !summary.unchecked_columns.is_empty() {
        text.push_str(&format!(
            " Concurrent changes to {} were not checked.",
            summary.unchecked_columns.join(", ")
        ));
    }
    text
}
