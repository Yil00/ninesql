//! Aggregate panel (F-DATA-09): count, sum, average, minimum and maximum of the selected cells,
//! per selected column, type-aware and bounded.
//!
//! - Numbers: integers are summed exactly (`i128`); a column holding floats or decimals is
//!   summed as `f64` and the result says so ([`ColumnAggregate::approximate`]).
//! - Text, dates, times, timestamps, booleans and binary values: count, minimum and maximum only
//!   (with the grid's type-aware comparison, [`crate::rows::compare`]); no sum, no average.
//! - At most [`MAX_AGGREGATE_CELLS`] cells are read; past it the result is flagged
//!   [`Aggregates::capped`] and covers the first cells only (I6: the panel never walks
//!   unbounded data, and never loads rows: only the loaded ones are selected).
//! - Values of a column the driver cut are flagged ([`ColumnAggregate::cut_values`]): their
//!   text minimum / maximum are prefixes (I8).

use std::cmp::Ordering;

use ninesql_core::transfer::value::{CellValue, cell};

use crate::format::{display_line, write_copy};
use crate::model::ResultSetModel;
use crate::rows::{RowView, compare};
use crate::selection::{ColumnOrder, GridSelection};

/// Cells read at most for one aggregate.
pub const MAX_AGGREGATE_CELLS: usize = 1_000_000;
/// Longest minimum / maximum text kept, in characters.
pub const MAX_EXTREME_CHARS: usize = 120;
/// Columns aggregated at most.
pub const MAX_AGGREGATE_COLUMNS: usize = 256;

/// The aggregates of one selected column.
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnAggregate {
    /// Column name.
    pub name: String,
    /// Selected cells read.
    pub cells: u64,
    /// Non-NULL values.
    pub count: u64,
    /// Sum, for numeric columns.
    pub sum: Option<f64>,
    /// Exact sum text for integer columns (no rounding).
    pub sum_text: Option<String>,
    /// Average, for numeric columns.
    pub avg: Option<f64>,
    /// Smallest value, as text (bounded).
    pub min: Option<String>,
    /// Largest value, as text (bounded).
    pub max: Option<String>,
    /// The sum and average are `f64` approximations (floats or decimals).
    pub approximate: bool,
    /// The column holds values cut by the driver.
    pub cut_values: bool,
}

/// The aggregates of a selection.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Aggregates {
    /// Per selected column, in display order.
    pub columns: Vec<ColumnAggregate>,
    /// Selected rows.
    pub rows: usize,
    /// The cell cap stopped the computation early.
    pub capped: bool,
    /// More columns were selected than [`MAX_AGGREGATE_COLUMNS`].
    pub columns_capped: bool,
}

impl Aggregates {
    /// Total non-NULL values over every column.
    #[must_use]
    pub fn total_count(&self) -> u64 {
        self.columns.iter().map(|c| c.count).sum()
    }
}

struct Acc<'a> {
    cells: u64,
    count: u64,
    int_sum: Option<i128>,
    float_sum: f64,
    numeric: bool,
    approximate: bool,
    non_numeric: bool,
    min: Option<(CellValue<'a>, usize)>,
    max: Option<(CellValue<'a>, usize)>,
}

/// Computes the aggregates of `selection` (display coordinates) over `model`, through the
/// client-side `rows` view and the column `order`.
#[must_use]
pub fn aggregate(
    model: &ResultSetModel,
    order: &ColumnOrder,
    rows: &RowView,
    selection: &GridSelection,
) -> Aggregates {
    let display_rows = selection.row_range(rows.len(model.row_count()));
    let mut cols: Vec<usize> = selection
        .col_range(order.len())
        .filter_map(|d| order.model(d))
        .collect();
    let columns_capped = cols.len() > MAX_AGGREGATE_COLUMNS;
    cols.truncate(MAX_AGGREGATE_COLUMNS);
    let mut out = Aggregates {
        rows: display_rows.len(),
        columns_capped,
        ..Aggregates::default()
    };
    if cols.is_empty() || display_rows.is_empty() {
        return out;
    }
    let mut accs: Vec<Acc<'_>> = cols
        .iter()
        .map(|_| Acc {
            cells: 0,
            count: 0,
            int_sum: Some(0),
            float_sum: 0.0,
            numeric: false,
            approximate: false,
            non_numeric: false,
            min: None,
            max: None,
        })
        .collect();
    let mut budget = MAX_AGGREGATE_CELLS;
    'rows: for d in display_rows {
        let Some(row) = rows.model_row(d) else {
            continue;
        };
        for (i, &c) in cols.iter().enumerate() {
            if budget == 0 {
                out.capped = true;
                break 'rows;
            }
            budget -= 1;
            let Some((array, r)) = model.array_at(row, c) else {
                continue;
            };
            let acc = &mut accs[i];
            acc.cells += 1;
            let v = cell(array, r);
            if matches!(v, CellValue::Null) {
                continue;
            }
            acc.count += 1;
            match &v {
                CellValue::Int(x) => {
                    acc.numeric = true;
                    acc.int_sum = acc.int_sum.and_then(|s| s.checked_add(i128::from(*x)));
                    acc.float_sum += *x as f64;
                }
                CellValue::UInt(x) => {
                    acc.numeric = true;
                    acc.int_sum = acc.int_sum.and_then(|s| s.checked_add(i128::from(*x)));
                    acc.float_sum += *x as f64;
                }
                CellValue::Float(x) => {
                    acc.numeric = true;
                    acc.approximate = true;
                    acc.int_sum = None;
                    acc.float_sum += *x;
                }
                CellValue::Decimal(s) => {
                    acc.numeric = true;
                    acc.approximate = true;
                    acc.int_sum = None;
                    acc.float_sum += s.parse::<f64>().unwrap_or(f64::NAN);
                }
                _ => acc.non_numeric = true,
            }
            if acc
                .min
                .as_ref()
                .is_none_or(|(m, _)| compare(&v, m) == Ordering::Less)
            {
                acc.min = Some((v.clone(), row));
            }
            if acc
                .max
                .as_ref()
                .is_none_or(|(m, _)| compare(&v, m) == Ordering::Greater)
            {
                acc.max = Some((v, row));
            }
        }
    }
    let text_at = |row: usize, c: usize| -> String {
        let mut s = String::new();
        if let Some((array, r)) = model.array_at(row, c) {
            write_copy(&mut s, array, r, "");
        }
        display_line(&s, MAX_EXTREME_CHARS).0
    };
    for (acc, &c) in accs.into_iter().zip(&cols) {
        let meta = &model.columns()[c];
        let numeric = acc.numeric && !acc.non_numeric && acc.count > 0;
        let sum = numeric.then_some(acc.float_sum);
        out.columns.push(ColumnAggregate {
            name: display_line(&meta.name, 80).0,
            cells: acc.cells,
            count: acc.count,
            sum,
            sum_text: if numeric && !acc.approximate {
                acc.int_sum.map(|s| s.to_string())
            } else {
                None
            },
            avg: sum.map(|s| s / acc.count as f64),
            min: acc.min.map(|(_, row)| text_at(row, c)),
            max: acc.max.map(|(_, row)| text_at(row, c)),
            approximate: numeric && acc.approximate,
            cut_values: meta.truncated_values,
        });
    }
    out
}

/// A number for the panel: integers without a fraction, others with up to 6 significant
/// decimals.
#[must_use]
pub fn number_text(v: f64) -> String {
    if !v.is_finite() {
        return v.to_string();
    }
    if v.fract() == 0.0 && v.abs() < 1e15 {
        return format!("{v:.0}");
    }
    let s = format!("{v:.6}");
    s.trim_end_matches('0').trim_end_matches('.').to_owned()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ninesql_core::driver_api::arrow_array::{Float64Array, Int64Array, RecordBatch};
    use ninesql_core::driver_api::arrow_schema::{DataType, Field, Schema};

    use super::*;
    use crate::model::tests::batch;
    use crate::rows::{ClientFilter, FilterOp};

    fn model(batches: Vec<RecordBatch>) -> ResultSetModel {
        let mut m = ResultSetModel::new(0, 0, batches[0].schema(), 1000);
        for b in batches {
            m.push_batch(b).unwrap();
        }
        m
    }

    #[test]
    fn integers_sum_exactly_and_text_gets_min_max_only() {
        let m = model(vec![batch(1, 10)]);
        let order = ColumnOrder::identity(2);
        let a = aggregate(&m, &order, &RowView::new(), &GridSelection::All);
        assert_eq!(a.rows, 10);
        let id = &a.columns[0];
        assert_eq!(id.count, 10);
        assert_eq!(id.sum_text.as_deref(), Some("55"));
        assert_eq!(id.avg, Some(5.5));
        assert_eq!(id.min.as_deref(), Some("1"));
        assert_eq!(id.max.as_deref(), Some("10"));
        assert!(!id.approximate);
        let name = &a.columns[1];
        assert_eq!(name.cells, 10);
        assert_eq!(name.count, 8, "NULLs at ids 3 and 10");
        assert_eq!(name.sum, None);
        assert_eq!(name.avg, None);
        assert_eq!(name.min.as_deref(), Some("n1"));
        assert_eq!(name.max.as_deref(), Some("n9"), "code point order");
    }

    #[test]
    fn floats_are_approximate() {
        let schema = Arc::new(Schema::new(vec![Field::new("x", DataType::Float64, true)]));
        let b = RecordBatch::try_new(
            schema,
            vec![Arc::new(Float64Array::from(vec![
                Some(0.5),
                None,
                Some(2.25),
            ]))],
        )
        .unwrap();
        let m = model(vec![b]);
        let a = aggregate(
            &m,
            &ColumnOrder::identity(1),
            &RowView::new(),
            &GridSelection::All,
        );
        let x = &a.columns[0];
        assert!(x.approximate);
        assert_eq!(x.sum, Some(2.75));
        assert_eq!(x.sum_text, None);
        assert_eq!(number_text(x.avg.unwrap()), "1.375");
    }

    #[test]
    fn selection_follows_the_row_view_and_column_order() {
        let m = model(vec![batch(0, 10)]);
        let mut order = ColumnOrder::identity(2);
        order.move_column(1, 0);
        let mut rows = RowView::new();
        rows.add_filter(ClientFilter::on(0, FilterOp::GreaterOrEqual, "8"), &m);
        // Display column 1 is now `id`; display rows 0..2 are ids 8 and 9.
        let sel = GridSelection::Cells {
            anchor: (0, 1),
            focus: (5, 1),
        };
        let a = aggregate(&m, &order, &rows, &sel);
        assert_eq!(a.rows, 2);
        assert_eq!(a.columns.len(), 1);
        assert_eq!(a.columns[0].name, "id");
        assert_eq!(a.columns[0].sum_text.as_deref(), Some("17"));
    }

    #[test]
    fn overflowing_integer_sum_falls_back_to_f64() {
        let schema = Arc::new(Schema::new(vec![Field::new("x", DataType::Int64, false)]));
        let b = RecordBatch::try_new(
            schema,
            vec![Arc::new(Int64Array::from(vec![i64::MAX, i64::MAX]))],
        )
        .unwrap();
        let m = model(vec![b]);
        let a = aggregate(
            &m,
            &ColumnOrder::identity(1),
            &RowView::new(),
            &GridSelection::All,
        );
        // i128 holds 2 × i64::MAX exactly.
        assert_eq!(
            a.columns[0].sum_text.as_deref(),
            Some("18446744073709551614")
        );
    }

    #[test]
    fn nothing_selected_gives_nothing() {
        let m = model(vec![batch(0, 3)]);
        let a = aggregate(
            &m,
            &ColumnOrder::identity(2),
            &RowView::new(),
            &GridSelection::None,
        );
        assert!(a.columns.is_empty());
        assert_eq!(a.total_count(), 0);
    }
}
