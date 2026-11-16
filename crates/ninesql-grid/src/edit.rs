//! Preparation of in-grid editing (F-DATA-04, P5-A): the pending-change model the grid shows as
//! dirty markers, and the [`RowEditor`] seam the core's row-editing service will implement.
//!
//! **Nothing here writes.** Saving (DML preview, transaction, production confirmation, I3) is
//! the core's: the grid hands [`EditBuffer::changes`] to the [`RowEditor`] and shows what comes
//! back. Until a `RowEditor` is attached, the grid is read-only and says why (I8).
//!
//! Rules the model enforces on its own:
//! - a column the driver cut (`ninesql.truncated`) or of a type the grid cannot render is never
//!   editable ([`ColumnMeta::editable`](crate::model::ColumnMeta::editable));
//! - a row is edited by its **model** row index (stable under the client-side sort and filter);
//! - a deleted row cannot be edited, an inserted row is only marked, never merged into Arrow;
//! - `Debug` never prints a value (values may be secrets typed by the user, I1).

use std::collections::BTreeMap;

use crate::model::ResultSetModel;

/// A new value typed for a cell.
#[derive(Clone, PartialEq, Eq)]
pub enum EditValue {
    /// SQL `NULL`.
    Null,
    /// Text as typed; the core converts it to the column's type (and refuses what does not
    /// convert).
    Text(String),
    /// The column's default (inserted rows only).
    Default,
}

impl std::fmt::Debug for EditValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Null => "Null",
            Self::Text(_) => "Text(<redacted>)",
            Self::Default => "Default",
        })
    }
}

/// A row of the result or a row added in the grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RowRef {
    /// A loaded row (model index).
    Loaded(usize),
    /// A row inserted in the grid (its sequence number).
    Inserted(u32),
}

/// The pending change of one row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowChange {
    /// New values of some columns (model column index → value).
    Update(BTreeMap<usize, EditValue>),
    /// A row to insert: one value per column (`Default` when not typed).
    Insert(Vec<EditValue>),
    /// The row is to be deleted.
    Delete,
}

/// Why an edit was refused by the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditRefused {
    /// No [`RowEditor`], or it declared the result read-only.
    ReadOnly,
    /// The column cannot be edited (cut by the driver, unsupported type, or not editable per
    /// the editor).
    ColumnNotEditable,
    /// The row is marked for deletion.
    RowDeleted,
    /// Outside the loaded rows or columns.
    OutOfRange,
    /// The editor does not declare this kind of change (insert / delete).
    NotSupported,
}

/// What the attached [`RowEditor`] allows for a result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Editability {
    /// Read-only, with the reason shown to the user ("no primary key or unique key", "the
    /// session is read-only", "the driver does not edit", "the result is not a single table").
    ReadOnly(String),
    /// Editable.
    Editable {
        /// Model columns that may be edited (key columns included when the driver allows it).
        columns: Vec<usize>,
        /// Model columns forming the row key the `WHERE` uses.
        key: Vec<usize>,
        /// Rows can be inserted.
        insert: bool,
        /// Rows can be deleted.
        delete: bool,
    },
}

/// The seam the core's row-editing service implements (P5-A core part). Every method returns
/// at once (I6); results come back through the grid's owner.
pub trait RowEditor {
    /// What can be edited in result `statement`. Called when the result's header arrives and
    /// when the session's state changes (read-only, transaction).
    fn editability(&self, statement: u32, model: &ResultSetModel) -> Editability;
    /// Asks for the DML preview of `changes` (the core generates it with the session dialect);
    /// the answer comes back with the statements to show before saving.
    fn preview(&self, statement: u32, changes: Vec<(RowRef, RowChange)>);
    /// Saves after the user accepted the preview (and the production confirmation, I3).
    fn save(&self, statement: u32, changes: Vec<(RowRef, RowChange)>);
}

/// Pending changes of one result.
#[derive(Debug, Clone, Default)]
pub struct EditBuffer {
    changes: BTreeMap<RowRef, RowChange>,
    next_insert: u32,
    editability: Option<Editability>,
}

impl EditBuffer {
    /// Read-only until [`set_editability`](Self::set_editability).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records what the editor allows (`None`: no editor attached).
    pub fn set_editability(&mut self, editability: Option<Editability>) {
        self.editability = editability;
    }

    /// Why the result is read-only, if it is.
    #[must_use]
    pub fn read_only_reason(&self) -> Option<&str> {
        match &self.editability {
            None => Some("Editing is not available yet for this result."),
            Some(Editability::ReadOnly(why)) => Some(why),
            Some(Editability::Editable { .. }) => None,
        }
    }

    /// Whether model column `col` can be edited in `model`.
    #[must_use]
    pub fn column_editable(&self, model: &ResultSetModel, col: usize) -> bool {
        let Some(Editability::Editable { columns, .. }) = &self.editability else {
            return false;
        };
        columns.contains(&col) && model.columns().get(col).is_some_and(|m| m.editable())
    }

    /// Sets the new value of a cell of a loaded row.
    ///
    /// # Errors
    /// [`EditRefused`].
    pub fn set_cell(
        &mut self,
        model: &ResultSetModel,
        row: usize,
        col: usize,
        value: EditValue,
    ) -> Result<(), EditRefused> {
        if self.read_only_reason().is_some() {
            return Err(EditRefused::ReadOnly);
        }
        if row >= model.row_count() || col >= model.columns().len() {
            return Err(EditRefused::OutOfRange);
        }
        if !self.column_editable(model, col) {
            return Err(EditRefused::ColumnNotEditable);
        }
        if value == EditValue::Default {
            return Err(EditRefused::NotSupported);
        }
        match self
            .changes
            .entry(RowRef::Loaded(row))
            .or_insert_with(|| RowChange::Update(BTreeMap::new()))
        {
            RowChange::Update(cols) => {
                cols.insert(col, value);
                Ok(())
            }
            RowChange::Delete => Err(EditRefused::RowDeleted),
            RowChange::Insert(_) => Err(EditRefused::OutOfRange),
        }
    }

    /// Drops the pending value of a cell.
    pub fn revert_cell(&mut self, row: usize, col: usize) {
        let key = RowRef::Loaded(row);
        if let Some(RowChange::Update(cols)) = self.changes.get_mut(&key) {
            cols.remove(&col);
            if cols.is_empty() {
                self.changes.remove(&key);
            }
        }
    }

    /// Marks a loaded row for deletion (its pending updates are dropped).
    ///
    /// # Errors
    /// [`EditRefused`].
    pub fn delete_row(&mut self, model: &ResultSetModel, row: usize) -> Result<(), EditRefused> {
        match &self.editability {
            Some(Editability::Editable { delete: true, .. }) => {}
            Some(Editability::Editable { .. }) => return Err(EditRefused::NotSupported),
            _ => return Err(EditRefused::ReadOnly),
        }
        if row >= model.row_count() {
            return Err(EditRefused::OutOfRange);
        }
        self.changes.insert(RowRef::Loaded(row), RowChange::Delete);
        Ok(())
    }

    /// Adds a row to insert, every value `Default`. Returns its reference.
    ///
    /// # Errors
    /// [`EditRefused`].
    pub fn insert_row(&mut self, model: &ResultSetModel) -> Result<RowRef, EditRefused> {
        match &self.editability {
            Some(Editability::Editable { insert: true, .. }) => {}
            Some(Editability::Editable { .. }) => return Err(EditRefused::NotSupported),
            _ => return Err(EditRefused::ReadOnly),
        }
        let r = RowRef::Inserted(self.next_insert);
        self.next_insert += 1;
        self.changes.insert(
            r,
            RowChange::Insert(vec![EditValue::Default; model.columns().len()]),
        );
        Ok(r)
    }

    /// Undoes every pending change of a row (an inserted row is removed).
    pub fn revert_row(&mut self, row: RowRef) {
        self.changes.remove(&row);
    }

    /// Drops every pending change (after a save or "discard").
    pub fn clear(&mut self) {
        self.changes.clear();
    }

    /// Whether a cell of a loaded row has a pending value (the dirty marker).
    #[must_use]
    pub fn is_cell_dirty(&self, row: usize, col: usize) -> bool {
        matches!(self.changes.get(&RowRef::Loaded(row)), Some(RowChange::Update(c)) if c.contains_key(&col))
    }

    /// Whether a loaded row is marked for deletion.
    #[must_use]
    pub fn is_row_deleted(&self, row: usize) -> bool {
        matches!(
            self.changes.get(&RowRef::Loaded(row)),
            Some(RowChange::Delete)
        )
    }

    /// The pending value of a cell.
    #[must_use]
    pub fn pending(&self, row: usize, col: usize) -> Option<&EditValue> {
        match self.changes.get(&RowRef::Loaded(row)) {
            Some(RowChange::Update(c)) => c.get(&col),
            _ => None,
        }
    }

    /// Whether anything is pending.
    #[must_use]
    pub fn is_dirty(&self) -> bool {
        !self.changes.is_empty()
    }

    /// Counts: (updated rows, inserted rows, deleted rows).
    #[must_use]
    pub fn counts(&self) -> (usize, usize, usize) {
        let mut out = (0, 0, 0);
        for c in self.changes.values() {
            match c {
                RowChange::Update(_) => out.0 += 1,
                RowChange::Insert(_) => out.1 += 1,
                RowChange::Delete => out.2 += 1,
            }
        }
        out
    }

    /// The pending changes, for [`RowEditor::preview`] / [`RowEditor::save`].
    #[must_use]
    pub fn changes(&self) -> Vec<(RowRef, RowChange)> {
        self.changes.iter().map(|(r, c)| (*r, c.clone())).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::tests::{batch, truncated_batch};

    fn model(b: ninesql_core::driver_api::arrow_array::RecordBatch) -> ResultSetModel {
        let mut m = ResultSetModel::new(0, 0, b.schema(), 100);
        m.push_batch(b).unwrap();
        m
    }

    fn editable(columns: Vec<usize>) -> Editability {
        Editability::Editable {
            columns,
            key: vec![0],
            insert: true,
            delete: true,
        }
    }

    #[test]
    fn read_only_without_an_editor() {
        let m = model(batch(0, 3));
        let mut e = EditBuffer::new();
        assert!(e.read_only_reason().is_some());
        assert_eq!(
            e.set_cell(&m, 0, 1, EditValue::Null),
            Err(EditRefused::ReadOnly)
        );
        e.set_editability(Some(Editability::ReadOnly("no key".into())));
        assert_eq!(e.read_only_reason(), Some("no key"));
        assert_eq!(e.insert_row(&m), Err(EditRefused::ReadOnly));
    }

    #[test]
    fn dirty_markers_follow_edits() {
        let m = model(batch(0, 3));
        let mut e = EditBuffer::new();
        e.set_editability(Some(editable(vec![1])));
        assert_eq!(
            e.set_cell(&m, 0, 0, EditValue::Text("9".into())),
            Err(EditRefused::ColumnNotEditable)
        );
        e.set_cell(&m, 1, 1, EditValue::Text("x".into())).unwrap();
        assert!(e.is_cell_dirty(1, 1));
        assert!(!e.is_cell_dirty(1, 0));
        assert_eq!(e.pending(1, 1), Some(&EditValue::Text("x".into())));
        assert_eq!(format!("{:?}", e.pending(1, 1)), "Some(Text(<redacted>))");
        e.revert_cell(1, 1);
        assert!(!e.is_dirty());
        assert_eq!(
            e.set_cell(&m, 9, 1, EditValue::Null),
            Err(EditRefused::OutOfRange)
        );
    }

    #[test]
    fn deletes_and_inserts_are_counted() {
        let m = model(batch(0, 3));
        let mut e = EditBuffer::new();
        e.set_editability(Some(editable(vec![1])));
        e.set_cell(&m, 2, 1, EditValue::Null).unwrap();
        e.delete_row(&m, 2).unwrap();
        assert!(e.is_row_deleted(2));
        assert!(!e.is_cell_dirty(2, 1), "a delete drops the row's updates");
        assert_eq!(
            e.set_cell(&m, 2, 1, EditValue::Null),
            Err(EditRefused::RowDeleted)
        );
        let r = e.insert_row(&m).unwrap();
        assert_eq!(r, RowRef::Inserted(0));
        assert_eq!(e.counts(), (0, 1, 1));
        assert_eq!(e.changes().len(), 2);
        e.revert_row(r);
        e.clear();
        assert!(!e.is_dirty());
    }

    #[test]
    fn cut_columns_are_never_editable() {
        let m = model(truncated_batch(0, 2));
        let mut e = EditBuffer::new();
        e.set_editability(Some(editable(vec![0, 1])));
        assert!(e.column_editable(&m, 0));
        assert!(!e.column_editable(&m, 1));
    }

    #[test]
    fn undeclared_kinds_are_refused() {
        let m = model(batch(0, 2));
        let mut e = EditBuffer::new();
        e.set_editability(Some(Editability::Editable {
            columns: vec![1],
            key: vec![0],
            insert: false,
            delete: false,
        }));
        assert_eq!(e.insert_row(&m), Err(EditRefused::NotSupported));
        assert_eq!(e.delete_row(&m, 0), Err(EditRefused::NotSupported));
    }
}
