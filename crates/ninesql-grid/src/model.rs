//! One tabular result set: its columns, the Arrow batches received so far, the paging state and
//! the memory they hold (I6).
//!
//! Rows are never copied out of Arrow: a cell is formatted from its batch only when it is drawn
//! or copied. Locating a row is a binary search over the batches' first-row offsets.

use std::ops::Range;
use std::sync::Arc;

use ninesql_core::driver_api::arrow_array::{ArrayRef, RecordBatch};
use ninesql_core::driver_api::arrow_schema::SchemaRef;
use ninesql_core::driver_api::{
    ENGINE_TYPE_METADATA_KEY, TRUNCATED_VALUES_METADATA_KEY, TruncationReason,
};

use crate::format::{CellText, DisplayOptions, ValueKind, format_display, type_label, write_copy};

/// One column of a result set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnMeta {
    /// Column name as the engine reported it.
    pub name: String,
    /// Arrow type label (`Int64`, `Utf8`…).
    pub arrow_type: String,
    /// The engine's type name when the Arrow type is not faithful (`ninesql.type`).
    pub engine_type: Option<String>,
    /// How values render.
    pub kind: ValueKind,
    /// At least one value of this column was cut by the driver (`ninesql.truncated` in any batch
    /// so far). Sticky for the result. Such a column is **not editable**: writing back a cut value
    /// destroys data (I8, ADR-0004).
    pub truncated_values: bool,
}

impl ColumnMeta {
    /// Whether an in-grid edit of this column could be offered (P5-A also needs the driver's
    /// editing capabilities and a unique key).
    #[must_use]
    pub fn editable(&self) -> bool {
        !self.truncated_values && self.kind != ValueKind::Unsupported
    }

    /// Header label: the name, plus a visible marker when values were cut.
    #[must_use]
    pub fn header_label(&self) -> String {
        if self.truncated_values {
            format!("{} [cut]", self.name)
        } else {
            self.name.clone()
        }
    }

    /// Type shown under the name: the engine type when known, else the Arrow type.
    #[must_use]
    pub fn type_name(&self) -> &str {
        self.engine_type.as_deref().unwrap_or(&self.arrow_type)
    }
}

/// Where a result stands with respect to fetching.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchState {
    /// Rows are being delivered until `target_rows` are loaded (`None`: until the end, "fetch
    /// all").
    Fetching {
        /// Loaded-row count at which the page is complete.
        target_rows: Option<u64>,
    },
    /// The page is loaded; more rows may follow on request.
    Idle,
    /// The stream ended: every row is loaded.
    Complete,
    /// The stream ended with `Truncated`: the driver-side budget was reached.
    Truncated(TruncationReason),
    /// The grid's own memory budget was reached; the grid released the stream.
    GridBudgetReached,
    /// The stream was dropped before its end; rows not loaded are unreachable.
    Closed,
    /// The execution service closed the result after it waited this many seconds with no
    /// fetch (`[execution] paused_result_idle_secs`, ADR-0024 §5): the rows loaded are kept,
    /// no more can be fetched.
    ClosedIdle {
        /// Seconds the result had been paused.
        after_secs: u64,
    },
    /// The statement failed or was cancelled after this result started.
    Stopped,
}

impl FetchState {
    /// More rows can be asked for.
    #[must_use]
    pub fn can_fetch(self) -> bool {
        matches!(self, Self::Idle)
    }

    /// Rows are on their way.
    #[must_use]
    pub fn is_fetching(self) -> bool {
        matches!(self, Self::Fetching { .. })
    }

    /// The result stopped before its end because of a memory budget (the banner case).
    #[must_use]
    pub fn budget_reached(self) -> bool {
        matches!(self, Self::Truncated(_) | Self::GridBudgetReached)
    }

    /// No more rows will ever arrive.
    #[must_use]
    pub fn is_final(self) -> bool {
        !matches!(self, Self::Fetching { .. } | Self::Idle)
    }
}

/// Why a batch was not kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchRejected {
    /// Its column count differs from the header's (a malformed stream).
    SchemaMismatch,
    /// The result no longer accepts rows (final state).
    NotAccepting,
}

/// One tabular result set.
#[derive(Debug, Clone)]
pub struct ResultSetModel {
    statement: u32,
    result_set: u32,
    schema: SchemaRef,
    columns: Vec<ColumnMeta>,
    batches: Vec<RecordBatch>,
    /// First global row of each batch (same length as `batches`).
    starts: Vec<usize>,
    rows: usize,
    bytes: u64,
    state: FetchState,
}

/// The estimated memory one batch holds, as the grid accounts it: Arrow's
/// `get_array_memory_size`, the measure [`ResultStream`](ninesql_core::driver_api::ResultStream)
/// counts against the budget, at least 64 bytes.
#[must_use]
pub fn batch_bytes(batch: &RecordBatch) -> u64 {
    (batch.get_array_memory_size() as u64).max(64)
}

impl ResultSetModel {
    /// A result set from its header, before any row. The first page is expected without a
    /// request (see [`crate::source`]), so the state starts as fetching `first_page_rows`.
    #[must_use]
    pub fn new(statement: u32, result_set: u32, schema: SchemaRef, first_page_rows: u32) -> Self {
        let columns = schema
            .fields()
            .iter()
            .map(|f| ColumnMeta {
                name: f.name().clone(),
                arrow_type: type_label(f.data_type()),
                engine_type: f.metadata().get(ENGINE_TYPE_METADATA_KEY).cloned(),
                kind: ValueKind::of_field(f),
                truncated_values: field_truncated(f),
            })
            .collect();
        Self {
            statement,
            result_set,
            schema,
            columns,
            batches: Vec::new(),
            starts: Vec::new(),
            rows: 0,
            bytes: 0,
            state: FetchState::Fetching {
                target_rows: Some(u64::from(first_page_rows)),
            },
        }
    }

    /// Statement number.
    #[must_use]
    pub fn statement(&self) -> u32 {
        self.statement
    }

    /// Result-set index within the statement.
    #[must_use]
    pub fn result_set(&self) -> u32 {
        self.result_set
    }

    /// The header's schema.
    #[must_use]
    pub fn schema(&self) -> &SchemaRef {
        &self.schema
    }

    /// Columns, in result order.
    #[must_use]
    pub fn columns(&self) -> &[ColumnMeta] {
        &self.columns
    }

    /// Rows loaded.
    #[must_use]
    pub fn row_count(&self) -> usize {
        self.rows
    }

    /// Batches held.
    #[must_use]
    pub fn batch_count(&self) -> usize {
        self.batches.len()
    }

    /// Estimated bytes held by the batches.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Paging state.
    #[must_use]
    pub fn state(&self) -> FetchState {
        self.state
    }

    /// Any column has cut values.
    #[must_use]
    pub fn has_truncated_values(&self) -> bool {
        self.columns.iter().any(|c| c.truncated_values)
    }

    /// Keeps a batch. The caller has checked the memory budget ([`batch_bytes`]). Columns the
    /// batch flags with `ninesql.truncated` become non-editable for the whole result.
    ///
    /// # Errors
    /// [`BatchRejected`] when the batch does not match the header or the result is final.
    pub fn push_batch(&mut self, batch: RecordBatch) -> Result<(), BatchRejected> {
        if self.state.is_final() {
            return Err(BatchRejected::NotAccepting);
        }
        if batch.num_columns() != self.columns.len() {
            return Err(BatchRejected::SchemaMismatch);
        }
        for (col, field) in self.columns.iter_mut().zip(batch.schema().fields()) {
            if field_truncated(field) {
                col.truncated_values = true;
            }
        }
        let n = batch.num_rows();
        self.bytes += batch_bytes(&batch);
        if n > 0 {
            self.starts.push(self.rows);
            self.rows += n;
            self.batches.push(batch);
        }
        if let FetchState::Fetching {
            target_rows: Some(target),
        } = self.state
            && self.rows as u64 >= target
        {
            self.state = FetchState::Idle;
        }
        Ok(())
    }

    /// Starts a fetch if one is possible, and returns the request's mode.
    pub fn begin_fetch(&mut self, mode: crate::FetchMode) -> bool {
        match (self.state, mode) {
            (FetchState::Idle, crate::FetchMode::Page { rows }) => {
                self.state = FetchState::Fetching {
                    target_rows: Some(self.rows as u64 + u64::from(rows)),
                };
                true
            }
            // "Fetch all" also upgrades a page in flight.
            (FetchState::Idle | FetchState::Fetching { .. }, crate::FetchMode::All) => {
                self.state = FetchState::Fetching { target_rows: None };
                true
            }
            _ => false,
        }
    }

    /// Records a final state (complete, truncated, closed…). A final state is never replaced,
    /// except that `Complete` / `Closed` do not hide an earlier budget truncation.
    pub fn finish(&mut self, state: FetchState) {
        if !self.state.is_final() {
            self.state = state;
        }
    }

    /// Batch index and row within it for global row `row`.
    #[must_use]
    pub fn locate(&self, row: usize) -> Option<(usize, usize)> {
        if row >= self.rows {
            return None;
        }
        let b = self.starts.partition_point(|&s| s <= row).checked_sub(1)?;
        Some((b, row - self.starts[b]))
    }

    /// The array of column `col` holding global row `row`, and the row within it.
    #[must_use]
    pub fn array_at(&self, row: usize, col: usize) -> Option<(&ArrayRef, usize)> {
        let (b, r) = self.locate(row)?;
        let batch = self.batches.get(b)?;
        (col < batch.num_columns()).then(|| (batch.column(col), r))
    }

    /// Display form of one cell; `None` outside the loaded rows.
    #[must_use]
    pub fn cell(&self, row: usize, col: usize, options: &DisplayOptions) -> Option<CellText> {
        let (array, r) = self.array_at(row, col)?;
        Some(format_display(array, r, self.columns[col].kind, options))
    }

    /// Appends the copy form of one cell (nothing outside the loaded rows).
    pub fn write_cell_copy(&self, out: &mut String, row: usize, col: usize, null_text: &str) {
        if let Some((array, r)) = self.array_at(row, col) {
            write_copy(out, array, r, null_text);
        }
    }

    /// Builds the visible window: display cells for `rows` × `cols` (model column indices, in
    /// display order). Only these cells are formatted; rows past the loaded ones are skipped.
    /// Walks batches sequentially, so a window costs one binary search, not one per row.
    #[must_use]
    pub fn window(
        &self,
        rows: Range<usize>,
        cols: &[usize],
        options: &DisplayOptions,
    ) -> Vec<Vec<CellText>> {
        let end = rows.end.min(self.rows);
        let mut out = Vec::with_capacity(end.saturating_sub(rows.start));
        let Some((mut b, mut r)) = self.locate(rows.start) else {
            return out;
        };
        for _ in rows.start..end {
            let batch = &self.batches[b];
            let line = cols
                .iter()
                .filter(|&&c| c < batch.num_columns())
                .map(|&c| format_display(batch.column(c), r, self.columns[c].kind, options))
                .collect();
            out.push(line);
            r += 1;
            if r >= batch.num_rows() {
                b += 1;
                r = 0;
            }
        }
        out
    }

    /// The batches, for adapters (export, record view). Shared, never copied.
    #[must_use]
    pub fn batches(&self) -> &[RecordBatch] {
        &self.batches
    }
}

fn field_truncated(field: &Arc<ninesql_core::driver_api::arrow_schema::Field>) -> bool {
    field
        .metadata()
        .get(TRUNCATED_VALUES_METADATA_KEY)
        .is_some_and(|v| v == "true")
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Arc;

    use ninesql_core::driver_api::arrow_array::{Int64Array, StringArray};
    use ninesql_core::driver_api::arrow_schema::{DataType, Field, Schema};

    use super::*;
    use crate::FetchMode;

    pub(crate) fn schema() -> SchemaRef {
        Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("name", DataType::Utf8, true),
        ]))
    }

    /// Rows `start..start + n`, names `n<id>` (every 7th NULL).
    pub(crate) fn batch(start: i64, n: usize) -> RecordBatch {
        let ids: Vec<i64> = (start..start + n as i64).collect();
        let names: Vec<Option<String>> = ids
            .iter()
            .map(|i| (i % 7 != 3).then(|| format!("n{i}")))
            .collect();
        RecordBatch::try_new(
            schema(),
            vec![
                Arc::new(Int64Array::from(ids)),
                Arc::new(StringArray::from(names)),
            ],
        )
        .unwrap()
    }

    pub(crate) fn truncated_batch(start: i64, n: usize) -> RecordBatch {
        let b = batch(start, n);
        let fields = vec![
            Field::new("id", DataType::Int64, false),
            Field::new("name", DataType::Utf8, true)
                .with_metadata([(TRUNCATED_VALUES_METADATA_KEY.to_owned(), "true".to_owned())]),
        ];
        RecordBatch::try_new(Arc::new(Schema::new(fields)), b.columns().to_vec()).unwrap()
    }

    #[test]
    fn locate_across_batches() {
        let mut m = ResultSetModel::new(0, 0, schema(), 10);
        m.push_batch(batch(0, 4)).unwrap();
        m.push_batch(batch(4, 0)).unwrap();
        m.push_batch(batch(4, 3)).unwrap();
        assert_eq!(m.row_count(), 7);
        assert_eq!(m.locate(0), Some((0, 0)));
        assert_eq!(m.locate(3), Some((0, 3)));
        assert_eq!(m.locate(4), Some((1, 0)));
        assert_eq!(m.locate(6), Some((1, 2)));
        assert_eq!(m.locate(7), None);
        let o = DisplayOptions::default();
        assert_eq!(m.cell(5, 0, &o).unwrap().text, "5");
        assert!(m.cell(3, 1, &o).unwrap().is_null);
        assert!(m.cell(9, 0, &o).is_none());
    }

    #[test]
    fn window_matches_cells() {
        let mut m = ResultSetModel::new(0, 0, schema(), 10);
        for i in 0..5 {
            m.push_batch(batch(i * 3, 3)).unwrap();
        }
        let o = DisplayOptions::default();
        let w = m.window(2..20, &[1, 0], &o);
        assert_eq!(w.len(), 13);
        for (i, line) in w.iter().enumerate() {
            assert_eq!(line[0], m.cell(2 + i, 1, &o).unwrap());
            assert_eq!(line[1], m.cell(2 + i, 0, &o).unwrap());
        }
        assert!(m.window(40..50, &[0], &o).is_empty());
    }

    #[test]
    fn paging_states() {
        let mut m = ResultSetModel::new(0, 0, schema(), 5);
        assert!(m.state().is_fetching());
        assert!(!m.begin_fetch(FetchMode::Page { rows: 5 }));
        m.push_batch(batch(0, 3)).unwrap();
        assert!(m.state().is_fetching());
        m.push_batch(batch(3, 3)).unwrap();
        assert_eq!(m.state(), FetchState::Idle);
        assert!(m.begin_fetch(FetchMode::Page { rows: 5 }));
        assert_eq!(
            m.state(),
            FetchState::Fetching {
                target_rows: Some(11)
            }
        );
        assert!(m.begin_fetch(FetchMode::All));
        m.push_batch(batch(6, 100)).unwrap();
        assert!(m.state().is_fetching(), "fetch all runs to the end");
        m.finish(FetchState::Truncated(TruncationReason::MaxBytes));
        m.finish(FetchState::Complete);
        assert_eq!(m.state(), FetchState::Truncated(TruncationReason::MaxBytes));
        assert!(m.state().budget_reached());
        assert_eq!(m.push_batch(batch(0, 1)), Err(BatchRejected::NotAccepting));
    }

    #[test]
    fn truncated_columns_are_sticky_and_not_editable() {
        let mut m = ResultSetModel::new(0, 0, schema(), 100);
        assert!(m.columns().iter().all(ColumnMeta::editable));
        m.push_batch(truncated_batch(0, 2)).unwrap();
        m.push_batch(batch(2, 2)).unwrap();
        assert!(m.columns()[1].truncated_values);
        assert!(!m.columns()[1].editable());
        assert!(m.columns()[0].editable());
        assert_eq!(m.columns()[1].header_label(), "name [cut]");
        assert!(m.has_truncated_values());
    }

    #[test]
    fn schema_mismatch_is_refused() {
        let mut m = ResultSetModel::new(0, 0, schema(), 100);
        let one = RecordBatch::try_new(
            Arc::new(Schema::new(vec![Field::new("x", DataType::Int64, false)])),
            vec![Arc::new(Int64Array::from(vec![1]))],
        )
        .unwrap();
        assert_eq!(m.push_batch(one), Err(BatchRejected::SchemaMismatch));
        assert_eq!(m.row_count(), 0);
    }

    #[test]
    fn bytes_are_accounted_per_batch() {
        let mut m = ResultSetModel::new(0, 0, schema(), 100);
        let b = batch(0, 50);
        let size = batch_bytes(&b);
        m.push_batch(b).unwrap();
        let empty = batch(0, 0);
        let empty_size = batch_bytes(&empty);
        assert!(empty_size >= 64);
        m.push_batch(empty).unwrap();
        assert_eq!(m.bytes(), size + empty_size);
        assert_eq!(m.batch_count(), 1);
    }
}
