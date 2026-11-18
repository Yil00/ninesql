//! Copy of a selection as TSV or CSV (F-DATA-01; the other formats of F-DATA-06 come in P5-A).
//!
//! - Values are copied in their copy form ([`crate::format::write_copy`]): full text as the
//!   driver delivered it, never the display cut.
//! - A field containing the delimiter, a quote, a line break or a carriage return is quoted and
//!   its quotes doubled (RFC 4180; spreadsheets read TSV the same way). NULL copies as an empty
//!   field.
//! - The text is bounded by [`MAX_COPY_BYTES`]: copy stops at a row boundary and reports it, so a
//!   copy never doubles the result's memory unboundedly (I6).
//! - **Formula guard** (on by default, [`CopyOptions::formula_guard`]): a header or a text
//!   value that a spreadsheet would run as a formula ([`needs_formula_guard`], the P5-C
//!   exporter's rule) gets a `'` in front, as in a CSV / TSV export (ADR-0027 §3.2). Values of a
//!   number column that read as numbers are never changed, so `-5` stays a number.
//! - Values of a column the driver cut (`ninesql.truncated`) are copied as their kept prefix;
//!   [`CopyOutput::cut_values`] says so, and the UI tells the user.

use ninesql_core::transfer::export::needs_formula_guard;

use crate::format::ValueKind;
use crate::model::ResultSetModel;
use crate::rows::RowView;
use crate::selection::{ColumnOrder, GridSelection};

/// Upper bound of a copied text: 64 MiB.
pub const MAX_COPY_BYTES: usize = 64 * 1024 * 1024;

/// Copy format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyFormat {
    /// Tab-separated values.
    Tsv,
    /// Comma-separated values.
    Csv,
}

impl CopyFormat {
    fn delimiter(self) -> char {
        match self {
            Self::Tsv => '\t',
            Self::Csv => ',',
        }
    }
}

/// Options of one copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CopyOptions {
    /// Format.
    pub format: CopyFormat,
    /// Start with a line of column names.
    pub headers: bool,
    /// Size bound of the text.
    pub max_bytes: usize,
    /// Spreadsheet formula guard ("CSV injection"; see the [module documentation](self)).
    pub formula_guard: bool,
}

impl CopyOptions {
    /// TSV or CSV, with or without headers, bounded by [`MAX_COPY_BYTES`], formula guard on.
    #[must_use]
    pub fn new(format: CopyFormat, headers: bool) -> Self {
        Self {
            format,
            headers,
            max_bytes: MAX_COPY_BYTES,
            formula_guard: true,
        }
    }
}

/// The copied text and what it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyOutput {
    /// The text for the clipboard.
    pub text: String,
    /// Rows copied (headers excluded).
    pub rows: usize,
    /// Columns copied.
    pub cols: usize,
    /// The size bound stopped the copy before the last selected row.
    pub capped: bool,
    /// At least one copied column holds values the driver cut.
    pub cut_values: bool,
}

/// Copies the selected cells of `model`. Returns `None` when nothing is selected.
#[must_use]
pub fn copy_selection(
    model: &ResultSetModel,
    order: &ColumnOrder,
    selection: &GridSelection,
    options: &CopyOptions,
) -> Option<CopyOutput> {
    copy_selection_in(model, order, &RowView::new(), selection, options)
}

/// [`copy_selection`] through a client-side sort / filter: display rows are mapped to model
/// rows, so what is copied is what is shown.
#[must_use]
pub fn copy_selection_in(
    model: &ResultSetModel,
    order: &ColumnOrder,
    view: &RowView,
    selection: &GridSelection,
    options: &CopyOptions,
) -> Option<CopyOutput> {
    let rows = selection.row_range(view.len(model.row_count()));
    let cols: Vec<usize> = selection
        .col_range(order.len())
        .filter_map(|d| order.model(d))
        .collect();
    if cols.is_empty() || (rows.is_empty() && !options.headers) {
        return None;
    }
    let delim = options.format.delimiter();
    let guard = options.formula_guard;
    let mut text = String::new();
    let mut field = String::new();
    let cut_values = cols.iter().any(|&c| model.columns()[c].truncated_values);
    let push_line = |text: &mut String, fields: &mut dyn Iterator<Item = &str>| {
        let mut first = true;
        for f in fields {
            if !first {
                text.push(delim);
            }
            first = false;
            push_field(text, f, delim, guard && needs_formula_guard(f));
        }
        text.push('\n');
    };
    if options.headers {
        let mut names = cols.iter().map(|&c| model.columns()[c].name.as_str());
        push_line(&mut text, &mut names);
    }
    let mut copied = 0usize;
    let mut capped = false;
    let mut line = String::new();
    for display in rows.clone() {
        let Some(row) = view.model_row(display) else {
            continue;
        };
        line.clear();
        for (i, &c) in cols.iter().enumerate() {
            if i > 0 {
                line.push(delim);
            }
            field.clear();
            model.write_cell_copy(&mut field, row, c, "");
            let guarded = guard
                && needs_formula_guard(&field)
                && !(model.columns()[c].kind == ValueKind::Number
                    && field.trim().parse::<f64>().is_ok());
            push_field(&mut line, &field, delim, guarded);
        }
        line.push('\n');
        if text.len() + line.len() > options.max_bytes {
            capped = true;
            break;
        }
        text.push_str(&line);
        copied += 1;
    }
    // No trailing newline for a single value: pasting one cell should not add a line.
    if copied == 1 && cols.len() == 1 && !options.headers {
        text.pop();
    }
    Some(CopyOutput {
        text,
        rows: copied,
        cols: cols.len(),
        capped,
        cut_values,
    })
}

/// Appends `value`, quoted when needed, with the formula guard's `'` in front when `guard`
/// (inside the quotes, as the exporter writes it).
fn push_field(out: &mut String, value: &str, delim: char, guard: bool) {
    if value.contains([delim, '"', '\n', '\r']) {
        out.push('"');
        if guard {
            out.push('\'');
        }
        for ch in value.chars() {
            if ch == '"' {
                out.push('"');
            }
            out.push(ch);
        }
        out.push('"');
    } else {
        if guard {
            out.push('\'');
        }
        out.push_str(value);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ninesql_core::driver_api::arrow_array::{RecordBatch, StringArray};
    use ninesql_core::driver_api::arrow_schema::{DataType, Field, Schema};

    use super::*;
    use crate::model::tests::{batch, schema, truncated_batch};

    fn model(rows: usize) -> ResultSetModel {
        let mut m = ResultSetModel::new(0, 0, schema(), 1);
        m.push_batch(batch(0, rows)).unwrap();
        m
    }

    #[test]
    fn tsv_with_headers_and_nulls() {
        let m = model(5);
        let order = ColumnOrder::identity(2);
        let sel = GridSelection::Rows {
            anchor: 2,
            focus: 4,
        };
        let out =
            copy_selection(&m, &order, &sel, &CopyOptions::new(CopyFormat::Tsv, true)).unwrap();
        assert_eq!(out.text, "id\tname\n2\tn2\n3\t\n4\tn4\n");
        assert_eq!((out.rows, out.cols, out.capped), (3, 2, false));
    }

    #[test]
    fn csv_quotes_and_reordered_columns() {
        let s = Arc::new(Schema::new(vec![Field::new("v", DataType::Utf8, true)]));
        let b = RecordBatch::try_new(
            s.clone(),
            vec![Arc::new(StringArray::from(vec![
                "a,b",
                "say \"hi\"",
                "line\nbreak",
                "plain",
            ]))],
        )
        .unwrap();
        let mut m = ResultSetModel::new(0, 0, s, 1);
        m.push_batch(b).unwrap();
        let out = copy_selection(
            &m,
            &ColumnOrder::identity(1),
            &GridSelection::All,
            &CopyOptions::new(CopyFormat::Csv, false),
        )
        .unwrap();
        assert_eq!(
            out.text,
            "\"a,b\"\n\"say \"\"hi\"\"\"\n\"line\nbreak\"\nplain\n"
        );
        // In TSV a comma needs no quoting.
        let out = copy_selection(
            &m,
            &ColumnOrder::identity(1),
            &GridSelection::cell(0, 0),
            &CopyOptions::new(CopyFormat::Tsv, false),
        )
        .unwrap();
        assert_eq!(out.text, "a,b");

        let m = model(2);
        let mut order = ColumnOrder::identity(2);
        order.move_column(1, 0);
        let out = copy_selection(
            &m,
            &order,
            &GridSelection::All,
            &CopyOptions::new(CopyFormat::Csv, true),
        )
        .unwrap();
        assert_eq!(out.text, "name,id\nn0,0\nn1,1\n");
    }

    #[test]
    fn copy_is_bounded_at_a_row_boundary() {
        let m = model(1000);
        let mut opts = CopyOptions::new(CopyFormat::Tsv, false);
        opts.max_bytes = 100;
        let out =
            copy_selection(&m, &ColumnOrder::identity(2), &GridSelection::All, &opts).unwrap();
        assert!(out.capped);
        assert!(out.text.len() <= 100);
        assert!(out.text.ends_with('\n'));
        assert_eq!(out.text.lines().count(), out.rows);
    }

    #[test]
    fn cut_values_are_reported_and_nothing_selected_is_none() {
        let mut m = ResultSetModel::new(0, 0, schema(), 1);
        m.push_batch(truncated_batch(0, 3)).unwrap();
        let order = ColumnOrder::identity(2);
        let opts = CopyOptions::new(CopyFormat::Tsv, false);
        let id_only = GridSelection::Columns {
            anchor: 0,
            focus: 0,
        };
        assert!(
            !copy_selection(&m, &order, &id_only, &opts)
                .unwrap()
                .cut_values
        );
        assert!(
            copy_selection(&m, &order, &GridSelection::All, &opts)
                .unwrap()
                .cut_values
        );
        assert!(copy_selection(&m, &order, &GridSelection::None, &opts).is_none());
    }

    #[test]
    fn formula_guard_is_on_by_default_for_text_and_headers() {
        use ninesql_core::driver_api::arrow_array::Int64Array;
        let s = Arc::new(Schema::new(vec![
            Field::new("=name", DataType::Utf8, true),
            Field::new("n", DataType::Int64, true),
        ]));
        let b = RecordBatch::try_new(
            s.clone(),
            vec![
                Arc::new(StringArray::from(vec![
                    Some("=1+2"),
                    Some("  @SUM(A1)"),
                    Some("\u{FF0B}x"),
                    Some("-a\tb"),
                    Some("plain"),
                    None,
                ])),
                Arc::new(Int64Array::from(vec![
                    Some(-5),
                    Some(1),
                    Some(2),
                    Some(3),
                    Some(-4),
                    None,
                ])),
            ],
        )
        .unwrap();
        let mut m = ResultSetModel::new(0, 0, s, 1);
        m.push_batch(b).unwrap();
        let order = ColumnOrder::identity(2);
        let opts = CopyOptions::new(CopyFormat::Tsv, true);
        assert!(opts.formula_guard, "on by default");
        let out = copy_selection(&m, &order, &GridSelection::All, &opts).unwrap();
        assert_eq!(
            out.text,
            "'=name\tn\n'=1+2\t-5\n'  @SUM(A1)\t1\n'\u{FF0B}x\t2\n\"'-a\tb\"\t3\nplain\t-4\n\t\n"
        );
        // A single cell is guarded too (pasted into a spreadsheet, it would run).
        let one = copy_selection(
            &m,
            &order,
            &GridSelection::cell(0, 0),
            &CopyOptions::new(CopyFormat::Tsv, false),
        )
        .unwrap();
        assert_eq!(one.text, "'=1+2");
        // CSV through this path gets the same guard; off, nothing changes.
        let csv = copy_selection(
            &m,
            &order,
            &GridSelection::cell(0, 0),
            &CopyOptions::new(CopyFormat::Csv, false),
        )
        .unwrap();
        assert_eq!(csv.text, "'=1+2");
        let mut raw = CopyOptions::new(CopyFormat::Tsv, true);
        raw.formula_guard = false;
        let out = copy_selection(&m, &order, &GridSelection::All, &raw).unwrap();
        assert!(out.text.starts_with("=name\tn\n=1+2\t-5\n"), "{}", out.text);
    }

    #[test]
    fn number_kind_text_is_guarded_unless_it_reads_as_a_number() {
        use std::collections::HashMap;
        // A text column the engine types as numeric (ValueKind::Number from `ninesql.type`).
        let field = Field::new("v", DataType::Utf8, true).with_metadata(HashMap::from([(
            ninesql_core::driver_api::ENGINE_TYPE_METADATA_KEY.to_owned(),
            "numeric".to_owned(),
        )]));
        let s = Arc::new(Schema::new(vec![field]));
        let b = RecordBatch::try_new(
            s.clone(),
            vec![Arc::new(StringArray::from(vec!["-12.50", "=HYPERLINK(1)"]))],
        )
        .unwrap();
        let mut m = ResultSetModel::new(0, 0, s, 1);
        m.push_batch(b).unwrap();
        assert_eq!(m.columns()[0].kind, ValueKind::Number);
        let out = copy_selection(
            &m,
            &ColumnOrder::identity(1),
            &GridSelection::All,
            &CopyOptions::new(CopyFormat::Tsv, false),
        )
        .unwrap();
        assert_eq!(out.text, "-12.50\n'=HYPERLINK(1)\n");
    }
}
