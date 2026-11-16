//! Client-side sort and filter over the **loaded rows only** (F-DATA-03, client part).
//!
//! A [`RowView`] is a permutation of the result's loaded rows: the grid draws display row `i`
//! from model row [`RowView::model_row`]`(i)`. Nothing is copied out of Arrow: the view holds
//! row indices only, and is rebuilt from the batches when rows arrive or the user changes the
//! sort or the filters. The work is bounded by the rows the grid holds, which the memory budget
//! bounds (I6).
//!
//! The label always says "loaded rows only" while a sort or a filter is active: rows not
//! loaded yet are neither sorted nor filtered, and the user must not read the view as the whole
//! result (I8). Sorting and filtering **on the server** (`ORDER BY` / `WHERE` rewriting) is a
//! core service; the grid only hands it a [`SortFilterSpec`] through [`ServerSortFilter`].
//!
//! # Comparison (type-aware)
//! - numbers compare numerically (integers exactly, decimals and floats as `f64`);
//! - booleans `false < true`; dates, times and timestamps by instant; binary byte-wise;
//! - text by code point (no locale collation, so it may differ from the engine's `ORDER BY`);
//! - other types by their copy text;
//! - `NULL` sorts **last** in both directions.

use std::cmp::Ordering;

use ninesql_core::driver_api::arrow_array::RecordBatch;
use ninesql_core::transfer::value::{CellValue, cell};

use crate::format::{ValueKind, write_copy};
use crate::model::ResultSetModel;

/// Sort direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortDirection {
    /// Smallest first.
    Ascending,
    /// Largest first.
    Descending,
}

impl SortDirection {
    /// `↑` / `↓`.
    #[must_use]
    pub fn arrow(self) -> &'static str {
        match self {
            Self::Ascending => "↑",
            Self::Descending => "↓",
        }
    }
}

/// Sort on one result column (model index).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SortKey {
    /// Result column (model index, not display position).
    pub column: usize,
    /// Direction.
    pub direction: SortDirection,
}

/// A filter operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterOp {
    /// The copy text contains the value, ignoring case.
    Contains,
    /// Equal (numbers numerically, booleans by truth value, else the copy text exactly).
    Equals,
    /// Not equal (NULL never matches, as in SQL).
    NotEquals,
    /// Greater than.
    Greater,
    /// Greater than or equal.
    GreaterOrEqual,
    /// Less than.
    Less,
    /// Less than or equal.
    LessOrEqual,
    /// Is NULL.
    IsNull,
    /// Is not NULL.
    IsNotNull,
}

impl FilterOp {
    /// Every operator, in menu order.
    pub const ALL: [Self; 9] = [
        Self::Contains,
        Self::Equals,
        Self::NotEquals,
        Self::Greater,
        Self::GreaterOrEqual,
        Self::Less,
        Self::LessOrEqual,
        Self::IsNull,
        Self::IsNotNull,
    ];

    /// Short label (`contains`, `=`, `is null`…).
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Contains => "contains",
            Self::Equals => "=",
            Self::NotEquals => "≠",
            Self::Greater => ">",
            Self::GreaterOrEqual => "≥",
            Self::Less => "<",
            Self::LessOrEqual => "≤",
            Self::IsNull => "is null",
            Self::IsNotNull => "is not null",
        }
    }

    /// Whether the operator takes a value.
    #[must_use]
    pub fn takes_value(self) -> bool {
        !matches!(self, Self::IsNull | Self::IsNotNull)
    }
}

/// One filter. `column: None` searches every column (only with [`FilterOp::Contains`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientFilter {
    /// Result column (model index), or every column.
    pub column: Option<usize>,
    /// Operator.
    pub op: FilterOp,
    /// Value as typed (ignored by `IsNull` / `IsNotNull`).
    pub value: String,
}

impl ClientFilter {
    /// A filter on one column.
    #[must_use]
    pub fn on(column: usize, op: FilterOp, value: impl Into<String>) -> Self {
        Self {
            column: Some(column),
            op,
            value: value.into(),
        }
    }

    /// "Any column contains `value`" (quick search).
    #[must_use]
    pub fn anywhere(value: impl Into<String>) -> Self {
        Self {
            column: None,
            op: FilterOp::Contains,
            value: value.into(),
        }
    }
}

/// Parses the filter bar's text for `column` (`None`: any column): `= v`, `!= v` (or `<> v`),
/// `> v`, `>= v`, `< v`, `<= v`, `is null`, `is not null`; anything else is "contains". A
/// leading `\` keeps the rest literal (`\>5` searches for `>5`). Without a column only
/// "contains" exists, so the whole text is the needle. `None` for an empty text.
#[must_use]
pub fn parse_filter_text(text: &str, column: Option<usize>) -> Option<ClientFilter> {
    let t = text.trim();
    if t.is_empty() {
        return None;
    }
    let Some(column) = column else {
        return Some(ClientFilter::anywhere(t));
    };
    if let Some(rest) = t.strip_prefix('\\') {
        return Some(ClientFilter::on(column, FilterOp::Contains, rest));
    }
    let lower = t.to_ascii_lowercase();
    if lower == "is null" {
        return Some(ClientFilter::on(column, FilterOp::IsNull, ""));
    }
    if lower == "is not null" {
        return Some(ClientFilter::on(column, FilterOp::IsNotNull, ""));
    }
    const OPS: [(&str, FilterOp); 7] = [
        (">=", FilterOp::GreaterOrEqual),
        ("<=", FilterOp::LessOrEqual),
        ("!=", FilterOp::NotEquals),
        ("<>", FilterOp::NotEquals),
        ("=", FilterOp::Equals),
        (">", FilterOp::Greater),
        ("<", FilterOp::Less),
    ];
    for (prefix, op) in OPS {
        if let Some(rest) = t.strip_prefix(prefix) {
            return Some(ClientFilter::on(column, op, rest.trim()));
        }
    }
    Some(ClientFilter::on(column, FilterOp::Contains, t))
}

/// What a server-side sort / filter would ask the core for: column names, never values from
/// other rows. The core turns it into `ORDER BY` / `WHERE` with the session dialect and bound
/// values (P5-A core part).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SortFilterSpec {
    /// Sort columns by name, in priority order.
    pub sort: Vec<(String, SortDirection)>,
    /// Filters by column name (`None`: any column).
    pub filters: Vec<(Option<String>, FilterOp, String)>,
}

/// The hook of server-side sort and filter: implemented by the editor tab over the core's
/// service once it exists. The grid offers "on the server" only when [`available`] says so
/// (I8).
///
/// [`available`]: ServerSortFilter::available
pub trait ServerSortFilter {
    /// Whether the active result can be re-run with a server-side sort / filter (a single read
    /// statement on a driver that supports it, no run in progress).
    fn available(&self) -> bool;
    /// Asks the core to re-run the result's statement with `spec`. Never blocks (I6).
    fn request(&self, statement: u32, spec: SortFilterSpec);
}

/// A permutation of the loaded rows: client-side sort and filters.
#[derive(Debug, Clone, Default)]
pub struct RowView {
    sort: Option<SortKey>,
    filters: Vec<ClientFilter>,
    /// Model rows in display order; `None` when no sort or filter is active.
    map: Option<Vec<usize>>,
    /// Model rows the map was built from.
    built_from: usize,
}

impl RowView {
    /// No sort, no filter: display row = model row.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A sort or a filter is active.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.sort.is_some() || !self.filters.is_empty()
    }

    /// The sort.
    #[must_use]
    pub fn sort(&self) -> Option<SortKey> {
        self.sort
    }

    /// The filters.
    #[must_use]
    pub fn filters(&self) -> &[ClientFilter] {
        &self.filters
    }

    /// Sets the sort (and rebuilds).
    pub fn set_sort(&mut self, sort: Option<SortKey>, model: &ResultSetModel) {
        self.sort = sort;
        self.rebuild(model);
    }

    /// Adds a filter (and rebuilds). A filter with no column and another operator than
    /// `Contains` is ignored.
    pub fn add_filter(&mut self, filter: ClientFilter, model: &ResultSetModel) {
        if filter.column.is_none() && filter.op != FilterOp::Contains {
            return;
        }
        if filter.column.is_some_and(|c| c >= model.columns().len()) {
            return;
        }
        self.filters.push(filter);
        self.rebuild(model);
    }

    /// Removes every filter, keeps the sort.
    pub fn clear_filters(&mut self, model: &ResultSetModel) {
        self.filters.clear();
        self.rebuild(model);
    }

    /// Removes the sort and the filters.
    pub fn clear(&mut self) {
        self.sort = None;
        self.filters.clear();
        self.map = None;
        self.built_from = 0;
    }

    /// Whether rows arrived since the last build.
    #[must_use]
    pub fn is_stale(&self, model: &ResultSetModel) -> bool {
        self.is_active() && self.built_from != model.row_count()
    }

    /// Rebuilds the permutation from the loaded rows (when active).
    pub fn rebuild(&mut self, model: &ResultSetModel) {
        self.built_from = model.row_count();
        if !self.is_active() {
            self.map = None;
            return;
        }
        let mut rows: Vec<usize> = (0..model.row_count())
            .filter(|&r| self.filters.iter().all(|f| matches(model, r, f)))
            .collect();
        if let Some(key) = self.sort
            && key.column < model.columns().len()
        {
            let values = column_values(model.batches(), key.column);
            rows.sort_by(|&a, &b| {
                let (va, vb) = (&values[a], &values[b]);
                match (va, vb) {
                    (CellValue::Null, CellValue::Null) => Ordering::Equal,
                    // NULL last in both directions.
                    (CellValue::Null, _) => Ordering::Greater,
                    (_, CellValue::Null) => Ordering::Less,
                    _ => {
                        let o = compare(va, vb);
                        match key.direction {
                            SortDirection::Ascending => o,
                            SortDirection::Descending => o.reverse(),
                        }
                    }
                }
            });
        }
        self.map = Some(rows);
    }

    /// Display rows.
    #[must_use]
    pub fn len(&self, model_rows: usize) -> usize {
        self.map.as_ref().map_or(model_rows, Vec::len)
    }

    /// Whether no row is shown.
    #[must_use]
    pub fn is_empty(&self, model_rows: usize) -> bool {
        self.len(model_rows) == 0
    }

    /// The model row of display row `display`.
    #[must_use]
    pub fn model_row(&self, display: usize) -> Option<usize> {
        match &self.map {
            Some(m) => m.get(display).copied(),
            None => Some(display),
        }
    }

    /// The label shown above the grid while active: what is sorted / filtered and that only
    /// loaded rows are.
    #[must_use]
    pub fn label(&self, model: &ResultSetModel) -> Option<String> {
        if !self.is_active() {
            return None;
        }
        let name = |c: usize| {
            model.columns().get(c).map_or_else(
                || "?".to_owned(),
                |m| crate::format::display_line(&m.name, 40).0,
            )
        };
        let mut parts = Vec::new();
        if let Some(s) = self.sort {
            parts.push(format!(
                "sorted by {} {}",
                name(s.column),
                s.direction.arrow()
            ));
        }
        for f in &self.filters {
            let col = f.column.map_or_else(|| "any column".to_owned(), name);
            if f.op.takes_value() {
                let (v, _) = crate::format::display_line(&f.value, 40);
                parts.push(format!("{col} {} \"{v}\"", f.op.label()));
            } else {
                parts.push(format!("{col} {}", f.op.label()));
            }
        }
        Some(format!(
            "Loaded rows only: {} · {} of {} loaded rows shown",
            parts.join(" · "),
            self.len(model.row_count()),
            model.row_count()
        ))
    }
}

fn column_values(batches: &[RecordBatch], column: usize) -> Vec<CellValue<'_>> {
    let mut out = Vec::with_capacity(batches.iter().map(RecordBatch::num_rows).sum());
    for b in batches {
        let array = b.column(column);
        for r in 0..b.num_rows() {
            out.push(cell(array, r));
        }
    }
    out
}

fn as_f64(v: &CellValue<'_>) -> Option<f64> {
    match v {
        CellValue::Int(i) => Some(*i as f64),
        CellValue::UInt(u) => Some(*u as f64),
        CellValue::Float(f) => Some(*f),
        CellValue::Decimal(d) => d.parse().ok(),
        _ => None,
    }
}

/// Type-aware comparison of two non-NULL values.
#[must_use]
pub fn compare(a: &CellValue<'_>, b: &CellValue<'_>) -> Ordering {
    use CellValue as V;
    match (a, b) {
        (V::Int(x), V::Int(y)) => x.cmp(y),
        (V::UInt(x), V::UInt(y)) => x.cmp(y),
        (V::Bool(x), V::Bool(y)) => x.cmp(y),
        (V::Text(x), V::Text(y)) => x.cmp(y),
        (V::Bytes(x), V::Bytes(y)) => x.cmp(y),
        (V::Date(x), V::Date(y)) => x.cmp(y),
        (
            V::Time {
                secs: s1,
                nanos: n1,
            },
            V::Time {
                secs: s2,
                nanos: n2,
            },
        )
        | (
            V::Timestamp {
                secs: s1,
                nanos: n1,
                ..
            },
            V::Timestamp {
                secs: s2,
                nanos: n2,
                ..
            },
        ) => (s1, n1).cmp(&(s2, n2)),
        _ if a.is_number() && b.is_number() => match (as_f64(a), as_f64(b)) {
            (Some(x), Some(y)) => x.total_cmp(&y),
            _ => text_of(a).cmp(&text_of(b)),
        },
        _ => text_of(a).cmp(&text_of(b)),
    }
}

fn text_of(v: &CellValue<'_>) -> String {
    match v {
        CellValue::Text(s) => (*s).to_owned(),
        CellValue::Decimal(s) | CellValue::Duration(s) | CellValue::Unsupported(s) => s.clone(),
        CellValue::Int(i) => i.to_string(),
        CellValue::UInt(u) => u.to_string(),
        CellValue::Float(f) => f.to_string(),
        CellValue::Bool(b) => b.to_string(),
        _ => String::new(),
    }
}

fn parse_bool(s: &str) -> Option<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "true" | "t" | "1" | "yes" | "y" => Some(true),
        "false" | "f" | "0" | "no" | "n" => Some(false),
        _ => None,
    }
}

/// Whether model row `row` passes `filter`.
#[must_use]
pub fn matches(model: &ResultSetModel, row: usize, filter: &ClientFilter) -> bool {
    let Some(column) = filter.column else {
        let needle = filter.value.to_lowercase();
        let mut text = String::new();
        return (0..model.columns().len()).any(|c| {
            text.clear();
            model.write_cell_copy(&mut text, row, c, "");
            text.to_lowercase().contains(&needle)
        });
    };
    let Some((array, r)) = model.array_at(row, column) else {
        return false;
    };
    let value = cell(array, r);
    let is_null = matches!(value, CellValue::Null);
    match filter.op {
        FilterOp::IsNull => return is_null,
        FilterOp::IsNotNull => return !is_null,
        _ if is_null => return false,
        _ => {}
    }
    let mut text = String::new();
    write_copy(&mut text, array, r, "");
    if filter.op == FilterOp::Contains {
        return text.to_lowercase().contains(&filter.value.to_lowercase());
    }
    let kind = model.columns()[column].kind;
    let wanted = filter.value.trim();
    let ord = if value.is_number() {
        match (as_f64(&value), wanted.parse::<f64>()) {
            (Some(x), Ok(y)) => {
                // Integers compare exactly when the value is an integer too.
                match (&value, wanted.parse::<i64>()) {
                    (CellValue::Int(i), Ok(w)) => i.cmp(&w),
                    _ => x.total_cmp(&y),
                }
            }
            // A value that is not a number never matches a numeric comparison.
            _ => return false,
        }
    } else if let CellValue::Bool(b) = value {
        match parse_bool(wanted) {
            Some(w) => b.cmp(&w),
            None => return false,
        }
    } else if kind == ValueKind::Temporal {
        // ISO 8601 copy text: comparing the common prefix orders dates and timestamps.
        let n = wanted.len().min(text.len());
        text.as_bytes()[..n]
            .cmp(&wanted.as_bytes()[..n])
            .then_with(|| {
                if matches!(filter.op, FilterOp::Equals | FilterOp::NotEquals) {
                    text.len().cmp(&wanted.len())
                } else {
                    Ordering::Equal
                }
            })
    } else {
        text.as_str().cmp(filter.value.as_str())
    };
    match filter.op {
        FilterOp::Equals => ord == Ordering::Equal,
        FilterOp::NotEquals => ord != Ordering::Equal,
        FilterOp::Greater => ord == Ordering::Greater,
        FilterOp::GreaterOrEqual => ord != Ordering::Less,
        FilterOp::Less => ord == Ordering::Less,
        FilterOp::LessOrEqual => ord != Ordering::Greater,
        FilterOp::Contains | FilterOp::IsNull | FilterOp::IsNotNull => false,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ninesql_core::driver_api::arrow_array::{
        BooleanArray, Float64Array, Int64Array, RecordBatch, StringArray,
    };
    use ninesql_core::driver_api::arrow_schema::{DataType, Field, Schema};

    use super::*;
    use crate::model::tests::batch;

    fn model(batches: Vec<RecordBatch>) -> ResultSetModel {
        let mut m = ResultSetModel::new(0, 0, batches[0].schema(), 1000);
        for b in batches {
            m.push_batch(b).unwrap();
        }
        m
    }

    fn mixed() -> ResultSetModel {
        let schema = Arc::new(Schema::new(vec![
            Field::new("n", DataType::Int64, true),
            Field::new("t", DataType::Utf8, true),
            Field::new("f", DataType::Float64, true),
            Field::new("b", DataType::Boolean, true),
        ]));
        let b = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(Int64Array::from(vec![Some(10), None, Some(2), Some(-5)])),
                Arc::new(StringArray::from(vec![
                    Some("banana"),
                    Some("Apple"),
                    None,
                    Some("cherry"),
                ])),
                Arc::new(Float64Array::from(vec![
                    Some(1.5),
                    Some(-0.5),
                    Some(10.0),
                    None,
                ])),
                Arc::new(BooleanArray::from(vec![
                    Some(true),
                    Some(false),
                    None,
                    Some(true),
                ])),
            ],
        )
        .unwrap();
        model(vec![b])
    }

    fn shown(v: &RowView, m: &ResultSetModel) -> Vec<usize> {
        (0..v.len(m.row_count()))
            .map(|i| v.model_row(i).unwrap())
            .collect()
    }

    #[test]
    fn inactive_view_is_identity() {
        let m = model(vec![batch(0, 5)]);
        let v = RowView::new();
        assert!(!v.is_active());
        assert_eq!(v.len(5), 5);
        assert_eq!(v.model_row(3), Some(3));
        assert!(v.label(&m).is_none());
    }

    #[test]
    fn numeric_sort_is_numeric_with_nulls_last() {
        let m = mixed();
        let mut v = RowView::new();
        v.set_sort(
            Some(SortKey {
                column: 0,
                direction: SortDirection::Ascending,
            }),
            &m,
        );
        assert_eq!(shown(&v, &m), vec![3, 2, 0, 1]);
        v.set_sort(
            Some(SortKey {
                column: 0,
                direction: SortDirection::Descending,
            }),
            &m,
        );
        assert_eq!(shown(&v, &m), vec![0, 2, 3, 1], "NULL stays last");
        v.set_sort(
            Some(SortKey {
                column: 2,
                direction: SortDirection::Ascending,
            }),
            &m,
        );
        assert_eq!(shown(&v, &m), vec![1, 0, 2, 3]);
    }

    #[test]
    fn text_sort_is_by_code_point() {
        let m = mixed();
        let mut v = RowView::new();
        v.set_sort(
            Some(SortKey {
                column: 1,
                direction: SortDirection::Ascending,
            }),
            &m,
        );
        assert_eq!(shown(&v, &m), vec![1, 0, 3, 2]);
        let label = v.label(&m).unwrap();
        assert!(label.starts_with("Loaded rows only"), "{label}");
        assert!(label.contains("sorted by t ↑"), "{label}");
    }

    #[test]
    fn sort_spans_batches_and_is_stable() {
        let m = model(vec![batch(0, 10), batch(10, 10)]);
        let mut v = RowView::new();
        v.set_sort(
            Some(SortKey {
                column: 0,
                direction: SortDirection::Descending,
            }),
            &m,
        );
        assert_eq!(v.model_row(0), Some(19));
        assert_eq!(v.model_row(19), Some(0));
    }

    #[test]
    fn filters_are_type_aware() {
        let m = mixed();
        let mut v = RowView::new();
        v.add_filter(ClientFilter::on(0, FilterOp::Greater, "2"), &m);
        assert_eq!(
            shown(&v, &m),
            vec![0],
            "10 > 2 numerically, NULL never matches"
        );
        v.clear_filters(&m);
        v.add_filter(ClientFilter::on(0, FilterOp::GreaterOrEqual, "2.0"), &m);
        assert_eq!(shown(&v, &m), vec![0, 2]);
        v.clear_filters(&m);
        v.add_filter(ClientFilter::on(0, FilterOp::Equals, "abc"), &m);
        assert!(shown(&v, &m).is_empty(), "not a number: no match");
        v.clear_filters(&m);
        v.add_filter(ClientFilter::on(1, FilterOp::Contains, "AN"), &m);
        assert_eq!(shown(&v, &m), vec![0], "contains ignores case");
        v.clear_filters(&m);
        v.add_filter(ClientFilter::on(1, FilterOp::IsNull, ""), &m);
        assert_eq!(shown(&v, &m), vec![2]);
        v.clear_filters(&m);
        v.add_filter(ClientFilter::on(3, FilterOp::Equals, "t"), &m);
        assert_eq!(shown(&v, &m), vec![0, 3]);
        v.clear_filters(&m);
        v.add_filter(ClientFilter::on(0, FilterOp::NotEquals, "10"), &m);
        assert_eq!(shown(&v, &m), vec![2, 3]);
    }

    #[test]
    fn quick_search_looks_in_every_column() {
        let m = mixed();
        let mut v = RowView::new();
        v.add_filter(ClientFilter::anywhere("10"), &m);
        assert_eq!(shown(&v, &m), vec![0, 2]);
        // Only `contains` searches every column.
        let mut w = RowView::new();
        w.add_filter(
            ClientFilter {
                column: None,
                op: FilterOp::Equals,
                value: "x".into(),
            },
            &m,
        );
        assert!(!w.is_active());
    }

    #[test]
    fn filter_text_is_parsed() {
        assert_eq!(parse_filter_text("  ", Some(0)), None);
        assert_eq!(
            parse_filter_text(">= 10", Some(1)),
            Some(ClientFilter::on(1, FilterOp::GreaterOrEqual, "10"))
        );
        assert_eq!(
            parse_filter_text("<>x", Some(1)),
            Some(ClientFilter::on(1, FilterOp::NotEquals, "x"))
        );
        assert_eq!(
            parse_filter_text("IS NOT NULL", Some(0)),
            Some(ClientFilter::on(0, FilterOp::IsNotNull, ""))
        );
        assert_eq!(
            parse_filter_text("\\>5", Some(0)),
            Some(ClientFilter::on(0, FilterOp::Contains, ">5"))
        );
        assert_eq!(
            parse_filter_text("abc", Some(0)),
            Some(ClientFilter::on(0, FilterOp::Contains, "abc"))
        );
        assert_eq!(
            parse_filter_text("> 3", None),
            Some(ClientFilter::anywhere("> 3"))
        );
    }

    #[test]
    fn new_rows_make_the_view_stale_until_rebuilt() {
        let mut m = ResultSetModel::new(0, 0, batch(0, 1).schema(), 10);
        m.push_batch(batch(0, 5)).unwrap();
        let mut v = RowView::new();
        v.add_filter(ClientFilter::on(0, FilterOp::Less, "3"), &m);
        assert_eq!(v.len(m.row_count()), 3);
        m.push_batch(batch(5, 5)).unwrap();
        assert!(v.is_stale(&m));
        v.rebuild(&m);
        assert!(!v.is_stale(&m));
        assert_eq!(v.len(m.row_count()), 3);
        assert!(v.label(&m).unwrap().contains("3 of 10 loaded rows shown"));
    }
}
