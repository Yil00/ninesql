//! Pending row changes of one grid result (F-DATA-04): cell edits, inserted, duplicated and
//! deleted rows, turned into [`DmlRequest`]s and a DML preview.
//!
//! Original values and key values are read from the grid's batches with [`cell_value`] when a
//! row is first touched, so the save compares against what the user saw
//! ([`Concurrency::EditedColumns`]). `Debug` never prints a value.

use std::collections::BTreeMap;
use std::fmt;

use ninesql_driver_api::arrow_array::RecordBatch;
use ninesql_driver_api::arrow_schema::{Field, Schema};
use ninesql_driver_api::dml::{
    ColumnChange, ColumnType, ColumnValue, Concurrency, DmlBatch, DmlColumn, DmlError, DmlRequest,
    cell_value, generate_batch,
};
use ninesql_driver_api::{
    Dialect, ENGINE_TYPE_METADATA_KEY, EditingCapabilities, Param, TRUNCATED_VALUES_METADATA_KEY,
};

use super::EditError;
use super::key::{EditableTable, NotEditable};
use super::query::QuerySource;

/// A row of the grid result, as the grid numbers it (its index in the result, stable while the
/// result lives).
pub type RowId = u64;

/// A row added in the grid (inserted or duplicated), numbered by [`PendingEdits`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NewRowId(u64);

impl NewRowId {
    /// The number.
    #[must_use]
    pub fn get(self) -> u64 {
        self.0
    }
}

/// Whether a result column can be edited.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ColumnStatus {
    /// It is a column of the edited table.
    Editable,
    /// No column of the table has this name (an expression or an alias).
    NotInTable,
    /// Two result columns have this name: neither is edited.
    Duplicate,
}

#[derive(Clone)]
struct ResultColumn {
    status: ColumnStatus,
    dml: DmlColumn,
    /// Engine type of the table column, for [`PendingEdits::parse_input`].
    engine_type: Option<String>,
    auto_generated: bool,
    key_part: bool,
}

#[derive(Clone, PartialEq)]
struct RowUpdate {
    key: Vec<Param>,
    /// Field index → (original, new).
    changes: BTreeMap<usize, (Param, Param)>,
}

#[derive(Clone, PartialEq)]
struct RowDelete {
    key: Vec<Param>,
    /// Field index → original value, for the concurrency guard.
    check: BTreeMap<usize, Param>,
}

/// The pending changes of one grid result on an [`EditableTable`]. Owned by the UI (one per
/// result tab); every method returns at once. See the [module documentation](self).
#[derive(Clone)]
pub struct PendingEdits {
    table: EditableTable,
    columns: Vec<ResultColumn>,
    /// Result field of each key column, in key order.
    key_fields: Vec<usize>,
    updates: BTreeMap<RowId, RowUpdate>,
    deletes: BTreeMap<RowId, RowDelete>,
    inserts: BTreeMap<NewRowId, BTreeMap<usize, Param>>,
    next_new: u64,
}

impl fmt::Debug for PendingEdits {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PendingEdits")
            .field("columns", &self.columns.len())
            .field("updates", &self.updates.len())
            .field("deletes", &self.deletes.len())
            .field("inserts", &self.inserts.len())
            .finish_non_exhaustive()
    }
}

fn truncated(field: &Field) -> bool {
    field
        .metadata()
        .get(TRUNCATED_VALUES_METADATA_KEY)
        .is_some_and(|v| v == "true")
}

impl PendingEdits {
    /// Starts editing a result on `table`. `source` is the [`QuerySource`] of the
    /// [`BuiltQuery`](super::BuiltQuery) that produced the result
    /// ([`EditService::table_query`](super::EditService::table_query), the navigator's "open
    /// table", foreign-key navigation) and `schema` the result-set header.
    ///
    /// The result is accepted only when its source is a query of `table` and its columns are
    /// exactly the ones the query selected, by name, in order and in number (for `SELECT *`,
    /// every column of `table` in ordinal order). Only the core creates a `QuerySource`, so a
    /// result of any other statement (an alias such as `SELECT other_unique AS id`, a join, an
    /// expression) cannot be edited. Result columns are then matched to table columns by name;
    /// the result must hold every key column exactly once.
    ///
    /// # Errors
    /// [`NotEditable::ResultNotFromTable`], [`NotEditable::KeyNotInResult`].
    pub fn new(
        table: EditableTable,
        source: &QuerySource,
        schema: &Schema,
    ) -> Result<Self, NotEditable> {
        if source.table() != table.table.as_slice() {
            return Err(NotEditable::ResultNotFromTable);
        }
        let names: Vec<&str> = schema.fields().iter().map(|f| f.name().as_str()).collect();
        let expected: Vec<&str> = match source.columns() {
            Some(cols) => cols.iter().map(String::as_str).collect(),
            None => table.columns.iter().map(|c| c.name.as_str()).collect(),
        };
        if names != expected {
            return Err(NotEditable::ResultNotFromTable);
        }
        let mut columns = Vec::with_capacity(schema.fields().len());
        for field in schema.fields() {
            let name = field.name();
            let same = schema.fields().iter().filter(|f| f.name() == name).count();
            let table_column = table.column(name);
            let status = match (table_column, same) {
                (_, n) if n > 1 => ColumnStatus::Duplicate,
                (None, _) => ColumnStatus::NotInTable,
                (Some(_), _) => ColumnStatus::Editable,
            };
            // The type the preview literals use: the result's Arrow type with the table's engine
            // type (the field's own when the driver set it).
            let mut ty = ColumnType::from_field(field);
            if ty.engine_type.is_none()
                && let Some(c) = table_column
            {
                ty = ty.with_engine_type(c.data_type.clone());
            }
            columns.push(ResultColumn {
                status,
                dml: DmlColumn::new(name.clone(), ty),
                engine_type: table_column
                    .map(|c| c.data_type.clone())
                    .or_else(|| field.metadata().get(ENGINE_TYPE_METADATA_KEY).cloned()),
                auto_generated: table_column.is_some_and(|c| c.auto_generated),
                key_part: table.key.columns.iter().any(|k| k == name),
            });
        }
        let mut key_fields = Vec::with_capacity(table.key.columns.len());
        let mut missing = Vec::new();
        for k in &table.key.columns {
            match columns
                .iter()
                .position(|c| c.status == ColumnStatus::Editable && c.dml.name == *k)
            {
                Some(i) => key_fields.push(i),
                None => missing.push(k.clone()),
            }
        }
        if !missing.is_empty() {
            return Err(NotEditable::KeyNotInResult(missing));
        }
        Ok(Self {
            table,
            columns,
            key_fields,
            updates: BTreeMap::new(),
            deletes: BTreeMap::new(),
            inserts: BTreeMap::new(),
            next_new: 1,
        })
    }

    /// The edited table.
    #[must_use]
    pub fn table(&self) -> &EditableTable {
        &self.table
    }

    /// Whether result column `field` can be edited ([`ColumnStatus`]). A batch may also flag a
    /// column's values as cut ([`TRUNCATED_VALUES_METADATA_KEY`]): such cells cannot be edited
    /// either ([`EditError::ValueCut`]).
    #[must_use]
    pub fn column_status(&self, field: usize) -> Option<ColumnStatus> {
        self.columns.get(field).map(|c| c.status)
    }

    /// Whether result column `field` is part of the row key.
    #[must_use]
    pub fn is_key_column(&self, field: usize) -> bool {
        self.columns.get(field).is_some_and(|c| c.key_part)
    }

    /// Converts what the user typed into a value for result column `field`, from the table
    /// column's engine type (`42` → integer, `2024-01-31` → date, `0x00ff` → bytes, text as
    /// typed). An empty input is an empty text (or an error for non-text types): `NULL` is set
    /// explicitly with [`Param::Null`].
    ///
    /// # Errors
    /// [`EditError::InvalidInput`] naming the expected type (never the input).
    pub fn parse_input(&self, field: usize, text: &str) -> Result<Param, EditError> {
        let column = self.columns.get(field).ok_or(EditError::NoSuchColumn)?;
        let ty = ninesql_transfer::TargetType::from_engine_type(
            column.engine_type.as_deref().unwrap_or("text"),
        );
        let mut options = ninesql_transfer::ValueOptions::default();
        options.null_text = None;
        options.empty_is_null = false;
        options.trim = ninesql_transfer::Trim::None;
        ninesql_transfer::import::convert(text, ty, &options)
            .map_err(|_| EditError::InvalidInput(ty.as_str()))
    }

    fn editable(&self, field: usize, batch: Option<&RecordBatch>) -> Result<(), EditError> {
        let column = self.columns.get(field).ok_or(EditError::NoSuchColumn)?;
        match column.status {
            ColumnStatus::Editable => {}
            ColumnStatus::NotInTable | ColumnStatus::Duplicate => {
                return Err(EditError::ColumnNotEditable(column.dml.name.clone()));
            }
        }
        if let Some(b) = batch
            && b.schema().fields().get(field).is_some_and(|f| truncated(f))
        {
            return Err(EditError::ValueCut(column.dml.name.clone()));
        }
        Ok(())
    }

    fn check_batch(&self, batch: &RecordBatch) -> Result<(), EditError> {
        let schema = batch.schema();
        let same = schema.fields().len() == self.columns.len()
            && schema
                .fields()
                .iter()
                .zip(&self.columns)
                .all(|(f, c)| *f.name() == c.dml.name);
        if same {
            Ok(())
        } else {
            Err(EditError::SchemaMismatch)
        }
    }

    fn read(batch: &RecordBatch, field: usize, row: usize) -> Result<Param, EditError> {
        cell_value(batch, field, row).map_err(|e| match e {
            DmlError::TruncatedValue(name) => EditError::ValueCut(name),
            other => EditError::Dml(other),
        })
    }

    fn key_of(&self, batch: &RecordBatch, row: usize) -> Result<Vec<Param>, EditError> {
        self.key_fields
            .iter()
            .map(|f| Self::read(batch, *f, row))
            .collect()
    }

    /// Sets result column `field` of grid row `row` (row `batch_row` of `batch`, a batch of this
    /// result) to `value`. Setting a cell back to its original value removes the change.
    ///
    /// # Errors
    /// [`EditError::RowDeleted`], [`EditError::ColumnNotEditable`], [`EditError::ValueCut`]
    /// (the cell or a key value was cut at the fetch budget), [`EditError::SchemaMismatch`].
    pub fn set_value(
        &mut self,
        row: RowId,
        batch: &RecordBatch,
        batch_row: usize,
        field: usize,
        value: Param,
    ) -> Result<(), EditError> {
        self.check_batch(batch)?;
        if self.deletes.contains_key(&row) {
            return Err(EditError::RowDeleted);
        }
        self.editable(field, Some(batch))?;
        let original = Self::read(batch, field, batch_row)?;
        let entry = match self.updates.get_mut(&row) {
            Some(u) => u,
            None => {
                let key = self.key_of(batch, batch_row)?;
                self.updates.entry(row).or_insert(RowUpdate {
                    key,
                    changes: BTreeMap::new(),
                })
            }
        };
        let original = entry
            .changes
            .get(&field)
            .map_or(original, |(o, _)| o.clone());
        if original == value {
            entry.changes.remove(&field);
        } else {
            entry.changes.insert(field, (original, value));
        }
        if entry.changes.is_empty() {
            self.updates.remove(&row);
        }
        Ok(())
    }

    /// The pending new value of a cell of an existing row, if it was edited.
    #[must_use]
    pub fn changed_value(&self, row: RowId, field: usize) -> Option<&Param> {
        self.updates
            .get(&row)
            .and_then(|u| u.changes.get(&field))
            .map(|(_, new)| new)
    }

    /// Undoes the edit of one cell.
    pub fn revert_cell(&mut self, row: RowId, field: usize) {
        if let Some(u) = self.updates.get_mut(&row) {
            u.changes.remove(&field);
            if u.changes.is_empty() {
                self.updates.remove(&row);
            }
        }
    }

    /// Undoes every change of an existing row (edits and deletion).
    pub fn revert_row(&mut self, row: RowId) {
        self.updates.remove(&row);
        self.deletes.remove(&row);
    }

    /// Marks grid row `row` for deletion (its pending edits are dropped). Original values of
    /// the row's columns are kept for the concurrency guard (columns whose values were cut are
    /// left out of it and reported as unchecked by the save).
    ///
    /// # Errors
    /// [`EditError::ValueCut`] when a key value was cut, [`EditError::SchemaMismatch`].
    pub fn delete_row(
        &mut self,
        row: RowId,
        batch: &RecordBatch,
        batch_row: usize,
    ) -> Result<(), EditError> {
        self.check_batch(batch)?;
        let key = self.key_of(batch, batch_row)?;
        let mut check = BTreeMap::new();
        for (i, c) in self.columns.iter().enumerate() {
            if c.status == ColumnStatus::Editable
                && !c.key_part
                && let Ok(v) = Self::read(batch, i, batch_row)
            {
                check.insert(i, v);
            }
        }
        self.updates.remove(&row);
        self.deletes.insert(row, RowDelete { key, check });
        Ok(())
    }

    /// Whether grid row `row` is marked for deletion.
    #[must_use]
    pub fn is_deleted(&self, row: RowId) -> bool {
        self.deletes.contains_key(&row)
    }

    /// Adds an empty row (every column at its default until set).
    pub fn insert_row(&mut self) -> NewRowId {
        let id = NewRowId(self.next_new);
        self.next_new += 1;
        self.inserts.insert(id, BTreeMap::new());
        id
    }

    /// Adds a copy of grid row `batch_row` of `batch`: every editable column but the
    /// engine-generated ones (identity, auto-increment), which get their default.
    ///
    /// # Errors
    /// [`EditError::ValueCut`] when a copied value was cut, [`EditError::SchemaMismatch`].
    pub fn duplicate_row(
        &mut self,
        batch: &RecordBatch,
        batch_row: usize,
    ) -> Result<NewRowId, EditError> {
        self.check_batch(batch)?;
        let mut values = BTreeMap::new();
        for (i, c) in self.columns.iter().enumerate() {
            if c.status == ColumnStatus::Editable && !c.auto_generated {
                values.insert(i, Self::read(batch, i, batch_row)?);
            }
        }
        let id = NewRowId(self.next_new);
        self.next_new += 1;
        self.inserts.insert(id, values);
        Ok(id)
    }

    /// Sets a column of an added row; [`None`] puts it back to the column's default.
    ///
    /// # Errors
    /// [`EditError::NoSuchRow`], [`EditError::ColumnNotEditable`].
    pub fn set_new_value(
        &mut self,
        row: NewRowId,
        field: usize,
        value: Option<Param>,
    ) -> Result<(), EditError> {
        self.editable(field, None)?;
        let values = self.inserts.get_mut(&row).ok_or(EditError::NoSuchRow)?;
        match value {
            Some(v) => values.insert(field, v),
            None => values.remove(&field),
        };
        Ok(())
    }

    /// The value set for a column of an added row (`None`: the default).
    #[must_use]
    pub fn new_value(&self, row: NewRowId, field: usize) -> Option<&Param> {
        self.inserts.get(&row).and_then(|v| v.get(&field))
    }

    /// Removes an added row.
    pub fn remove_new_row(&mut self, row: NewRowId) {
        self.inserts.remove(&row);
    }

    /// The added rows, in the order they were added.
    #[must_use]
    pub fn new_rows(&self) -> Vec<NewRowId> {
        self.inserts.keys().copied().collect()
    }

    /// Drops every pending change.
    pub fn clear(&mut self) {
        self.updates.clear();
        self.deletes.clear();
        self.inserts.clear();
    }

    /// `true` when nothing is pending.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.updates.is_empty() && self.deletes.is_empty() && self.inserts.is_empty()
    }

    /// Number of statements a save would run (one per changed, deleted or added row).
    #[must_use]
    pub fn change_count(&self) -> usize {
        self.updates.len() + self.deletes.len() + self.inserts.len()
    }

    fn key_values(&self, key: &[Param]) -> Vec<ColumnValue> {
        self.key_fields
            .iter()
            .zip(key)
            .map(|(f, v)| ColumnValue::new(self.columns[*f].dml.clone(), v.clone()))
            .collect()
    }

    /// The row changes as P5-B requests, in save order: deletions, then updates, then
    /// insertions (so a key freed by a deletion can be reused). `UPDATE` and `DELETE` use
    /// [`Concurrency::EditedColumns`].
    #[must_use]
    pub fn requests(&self) -> Vec<DmlRequest> {
        let table = self.table.table.clone();
        let kind = self.table.key.kind;
        let mut out = Vec::with_capacity(self.change_count());
        for d in self.deletes.values() {
            let check = d
                .check
                .iter()
                .map(|(f, v)| ColumnValue::new(self.columns[*f].dml.clone(), v.clone()))
                .collect();
            out.push(
                DmlRequest::delete(table.clone(), kind, self.key_values(&d.key))
                    .with_concurrency(Concurrency::EditedColumns)
                    .with_delete_check(check),
            );
        }
        for u in self.updates.values() {
            let changes = u
                .changes
                .iter()
                .map(|(f, (original, new))| {
                    ColumnChange::new(self.columns[*f].dml.clone(), original.clone(), new.clone())
                })
                .collect();
            out.push(
                DmlRequest::update(table.clone(), kind, self.key_values(&u.key), changes)
                    .with_concurrency(Concurrency::EditedColumns),
            );
        }
        for values in self.inserts.values() {
            let values = values
                .iter()
                .map(|(f, v)| ColumnValue::new(self.columns[*f].dml.clone(), v.clone()))
                .collect();
            out.push(DmlRequest::insert(table.clone(), values));
        }
        out
    }

    /// The statements a save would run, with `dialect` (the session's) and the engine's editing
    /// capabilities: [`SavePlan::script`] is the DML preview shown before saving.
    ///
    /// # Errors
    /// [`EditError::NothingToSave`]; [`EditError::TooManyChanges`] above
    /// [`MAX_CHANGES_PER_SAVE`](super::MAX_CHANGES_PER_SAVE); [`EditError::Dml`] for a change the generator refuses
    /// (a `NULL` key value, a value with no faithful literal, an edit kind the engine does not
    /// support…).
    pub fn plan(
        &self,
        dialect: &dyn Dialect,
        editing: &EditingCapabilities,
    ) -> Result<SavePlan, EditError> {
        if self.is_empty() {
            return Err(EditError::NothingToSave);
        }
        if self.change_count() > super::MAX_CHANGES_PER_SAVE {
            return Err(EditError::TooManyChanges {
                max: super::MAX_CHANGES_PER_SAVE,
            });
        }
        let batch = generate_batch(dialect, editing, &self.requests())?;
        let mut unchecked: Vec<String> = Vec::new();
        for s in &batch.statements {
            for c in &s.unchecked_columns {
                if !unchecked.contains(c) {
                    unchecked.push(c.clone());
                }
            }
        }
        // Columns whose values were cut are never compared on delete.
        for d in self.deletes.values() {
            for (i, c) in self.columns.iter().enumerate() {
                if c.status == ColumnStatus::Editable
                    && !c.key_part
                    && !d.check.contains_key(&i)
                    && !unchecked.contains(&c.dml.name)
                {
                    unchecked.push(c.dml.name.clone());
                }
            }
        }
        Ok(SavePlan {
            script: batch.preview_script(),
            table: dialect.qualified_name(&self.table.table),
            batch,
            unchecked_columns: unchecked,
            deletes: self.deletes.len(),
            updates: self.updates.len(),
            inserts: self.inserts.len(),
        })
    }
}

/// The statements of a save ([`PendingEdits::plan`]). `Debug` shows counts only.
#[derive(Clone, PartialEq)]
#[non_exhaustive]
pub struct SavePlan {
    /// The generated statements (bound forms run by the save), in order.
    pub batch: DmlBatch,
    /// The DML preview: every statement with literals, one per line, `;`-terminated.
    pub script: String,
    /// The table, quoted as the statements spell it.
    pub table: String,
    /// Columns left out of the concurrency guard (no reliable equality on the engine, or values
    /// cut at the fetch budget): a concurrent change of these is not detected.
    pub unchecked_columns: Vec<String>,
    /// Rows deleted.
    pub deletes: usize,
    /// Rows updated.
    pub updates: usize,
    /// Rows inserted.
    pub inserts: usize,
}

impl fmt::Debug for SavePlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SavePlan")
            .field("statements", &self.batch.statements.len())
            .field("script_len", &self.script.len())
            .field("unchecked_columns", &self.unchecked_columns.len())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).
mod tests {
    use std::sync::Arc;

    use ninesql_driver_api::arrow_array::{ArrayRef, Int64Array, StringArray};
    use ninesql_driver_api::arrow_schema::DataType;
    use ninesql_driver_api::dml::KeyKind;
    use ninesql_driver_api::{AnsiDialect, ObjectKind, ObjectPath};

    use super::super::key::{RowKey, TableColumn};
    use super::*;

    fn column(name: &str, ty: &str, auto: bool) -> TableColumn {
        TableColumn {
            name: name.into(),
            data_type: ty.into(),
            nullable: true,
            primary_key: name == "id",
            auto_generated: auto,
            has_default: auto,
        }
    }

    pub(crate) fn table() -> EditableTable {
        EditableTable {
            path: ObjectPath::root()
                .child(ObjectKind::Schema, "s")
                .child(ObjectKind::Table, "t"),
            table: vec!["s".into(), "t".into()],
            columns: vec![column("id", "bigint", true), column("label", "text", false)],
            key: RowKey {
                columns: vec!["id".into()],
                kind: KeyKind::PrimaryKey,
            },
            transactional: Some(true),
        }
    }

    fn batch(extra: bool) -> RecordBatch {
        let mut fields = vec![
            Field::new("id", DataType::Int64, false),
            Field::new("label", DataType::Utf8, true),
        ];
        let mut cols: Vec<ArrayRef> = vec![
            Arc::new(Int64Array::from(vec![1, 2])),
            Arc::new(StringArray::from(vec![Some("a"), None])),
        ];
        if extra {
            fields.push(Field::new("upper", DataType::Utf8, true));
            cols.push(Arc::new(StringArray::from(vec![Some("A"), None])));
        }
        RecordBatch::try_new(Arc::new(Schema::new(fields)), cols).unwrap()
    }

    fn src(table: &[&str], cols: Option<&[&str]>) -> QuerySource {
        let mut q = super::super::TableQuery::new(table.iter().map(|s| (*s).to_owned()).collect());
        if let Some(c) = cols {
            q = q.with_columns(c.iter().map(|s| (*s).to_owned()).collect());
        }
        super::super::build_table_query(&AnsiDialect, &q)
            .unwrap()
            .source()
            .clone()
    }

    fn star() -> QuerySource {
        src(&["s", "t"], None)
    }

    fn all() -> EditingCapabilities {
        ninesql_driver_api::mock::full_capabilities().editing
    }

    #[test]
    fn edits_plan_in_order_with_guards() {
        let b = batch(true);
        let source = src(&["s", "t"], Some(&["id", "label", "upper"]));
        let mut p = PendingEdits::new(table(), &source, &b.schema()).unwrap();
        assert_eq!(p.column_status(2), Some(ColumnStatus::NotInTable));
        assert!(p.set_value(0, &b, 0, 2, Param::Text("x".into())).is_err());
        p.set_value(0, &b, 0, 1, Param::Text("new".into())).unwrap();
        p.delete_row(1, &b, 1).unwrap();
        let n = p.duplicate_row(&b, 0).unwrap();
        assert_eq!(
            p.new_value(n, 0),
            None,
            "identity column left to its default"
        );
        assert_eq!(p.new_value(n, 1), Some(&Param::Text("a".into())));
        let plan = p.plan(&AnsiDialect, &all()).unwrap();
        assert_eq!(
            plan.script,
            "DELETE FROM \"s\".\"t\" WHERE \"id\" = 2 AND \"label\" IS NULL;\n\
             UPDATE \"s\".\"t\" SET \"label\" = 'new' WHERE \"id\" = 1 AND \"label\" = 'a';\n\
             INSERT INTO \"s\".\"t\" (\"label\") VALUES ('a');\n"
        );
        assert_eq!(plan.batch.statements[1].statement.params.len(), 3);
        assert!(!format!("{p:?} {plan:?}").contains("new"));
        // Back to the original value: no change left for that row.
        p.set_value(0, &b, 0, 1, Param::Text("a".into())).unwrap();
        assert_eq!(p.change_count(), 2);
        p.clear();
        assert!(matches!(
            p.plan(&AnsiDialect, &all()),
            Err(EditError::NothingToSave)
        ));
    }

    #[test]
    fn key_must_be_in_the_result_and_cut_values_are_refused() {
        let schema = Schema::new(vec![Field::new("label", DataType::Utf8, true)]);
        assert_eq!(
            PendingEdits::new(table(), &src(&["s", "t"], Some(&["label"])), &schema).unwrap_err(),
            NotEditable::KeyNotInResult(vec!["id".into()])
        );
        let cut = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("label", DataType::Utf8, true)
                .with_metadata([(TRUNCATED_VALUES_METADATA_KEY.to_owned(), "true".to_owned())]),
        ]));
        let b = RecordBatch::try_new(cut, batch(false).columns().to_vec()).unwrap();
        let mut p = PendingEdits::new(table(), &star(), &b.schema()).unwrap();
        assert!(matches!(
            p.set_value(0, &b, 0, 1, Param::Null),
            Err(EditError::ValueCut(_))
        ));
        // A delete leaves the cut column out of its guard and reports it.
        p.delete_row(0, &b, 0).unwrap();
        let plan = p.plan(&AnsiDialect, &all()).unwrap();
        assert_eq!(plan.unchecked_columns, vec!["label".to_owned()]);
        assert_eq!(plan.script, "DELETE FROM \"s\".\"t\" WHERE \"id\" = 1;\n");
    }

    #[test]
    fn parse_input_follows_the_engine_type() {
        let b = batch(false);
        let p = PendingEdits::new(table(), &star(), &b.schema()).unwrap();
        assert_eq!(p.parse_input(0, "42").unwrap(), Param::Int(42));
        assert!(matches!(
            p.parse_input(0, "x"),
            Err(EditError::InvalidInput("integer"))
        ));
        assert_eq!(p.parse_input(1, "").unwrap(), Param::Text(String::new()));
    }

    #[test]
    fn results_not_from_the_tables_query_are_refused() {
        let b = batch(false);
        // `SELECT other AS id, label FROM s.t` has the right names but no source of its own;
        // a source of the same columns is required, and its columns must match exactly.
        let renamed = Schema::new(vec![
            Field::new("label", DataType::Utf8, true),
            Field::new("id", DataType::Int64, false),
        ]);
        assert_eq!(
            PendingEdits::new(table(), &star(), &renamed).unwrap_err(),
            NotEditable::ResultNotFromTable,
            "columns out of order"
        );
        assert_eq!(
            PendingEdits::new(table(), &src(&["s", "u"], None), &b.schema()).unwrap_err(),
            NotEditable::ResultNotFromTable,
            "another table"
        );
        assert_eq!(
            PendingEdits::new(
                table(),
                &src(&["s", "t"], Some(&["label", "id"])),
                &b.schema()
            )
            .unwrap_err(),
            NotEditable::ResultNotFromTable,
            "other selected columns"
        );
        assert_eq!(
            PendingEdits::new(table(), &star(), &batch(true).schema()).unwrap_err(),
            NotEditable::ResultNotFromTable,
            "an extra column"
        );
        assert!(PendingEdits::new(table(), &star(), &b.schema()).is_ok());
    }
}
