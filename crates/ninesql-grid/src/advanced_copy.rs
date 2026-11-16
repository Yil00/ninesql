//! Advanced copy (F-DATA-06): the selection as CSV, TSV, Markdown, JSON or SQL `INSERT`.
//!
//! - CSV, TSV, Markdown and JSON go through the P5-C exporter ([`Exporter`]), so the clipboard
//!   gets exactly the escaping of an export file: RFC 4180 quoting, Markdown and JSON escaping,
//!   and the spreadsheet **formula guard** on by default for CSV and TSV ("CSV injection").
//!   The selected rows are copied out of the batches in chunks of [`CHUNK_ROWS`] rows, so the
//!   intermediate memory stays small whatever the selection.
//! - SQL `INSERT` uses `ninesql_driver_api::dml::insert_preview` with the **session's
//!   dialect**, one statement per row, and needs the table the rows go to: the caller passes
//!   it (navigator context) or asks the user; without one the copy is refused
//!   ([`CopyError::NoTable`]). Values come from `dml::cell_value`, which refuses a column the
//!   driver cut (writing a prefix back as if it were the value destroys data, I8).
//! - The text is bounded by [`MAX_COPY_BYTES`], checked after each chunk: the copy stops at a
//!   row boundary and says so ([`CopyOutput::capped`]).
//! - Rows follow the client-side sort / filter ([`RowView`]) and the column order: what is
//!   copied is what is shown.

use std::sync::Arc;

use arrow_data::transform::MutableArrayData;
use ninesql_core::driver_api::Dialect;
use ninesql_core::driver_api::arrow_array::{ArrayRef, RecordBatch, make_array};
use ninesql_core::driver_api::arrow_schema::{Schema, SchemaRef};
use ninesql_core::driver_api::dml::{ColumnValue, DmlColumn, cell_value, insert_preview};
use ninesql_core::transfer::{ExportFormat, ExportOptions, Exporter};

use crate::copy::{CopyOutput, MAX_COPY_BYTES};
use crate::model::ResultSetModel;
use crate::rows::RowView;
use crate::selection::{ColumnOrder, GridSelection};

/// Rows copied out of the batches at a time.
pub const CHUNK_ROWS: usize = 256;

/// A "Copy as …" format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AdvancedFormat {
    /// Comma-separated values with a header line.
    Csv,
    /// Tab-separated values with a header line.
    Tsv,
    /// A Markdown table.
    Markdown,
    /// A JSON array of objects.
    Json,
    /// One SQL `INSERT` per row.
    SqlInsert,
}

impl AdvancedFormat {
    /// Every format, in menu order.
    pub const ALL: [Self; 5] = [
        Self::Csv,
        Self::Tsv,
        Self::Markdown,
        Self::Json,
        Self::SqlInsert,
    ];

    /// Menu label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Csv => "CSV",
            Self::Tsv => "TSV",
            Self::Markdown => "Markdown",
            Self::Json => "JSON",
            Self::SqlInsert => "SQL INSERT",
        }
    }

    fn export_format(self) -> Option<ExportFormat> {
        match self {
            Self::Csv => Some(ExportFormat::Csv),
            Self::Tsv => Some(ExportFormat::Tsv),
            Self::Markdown => Some(ExportFormat::Markdown),
            Self::Json => Some(ExportFormat::JsonArray),
            Self::SqlInsert => None,
        }
    }
}

/// Where SQL `INSERT`s go.
#[derive(Clone)]
pub struct InsertTarget {
    /// The session's dialect (quoting and literals).
    pub dialect: Arc<dyn Dialect>,
    /// Table name parts (`[schema, table]`), unquoted.
    pub table: Vec<String>,
}

impl std::fmt::Debug for InsertTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InsertTarget")
            .field("table_parts", &self.table.len())
            .finish_non_exhaustive()
    }
}

/// Options of one advanced copy.
#[derive(Debug, Clone)]
pub struct AdvancedCopy {
    /// Format.
    pub format: AdvancedFormat,
    /// Spreadsheet formula guard for CSV / TSV (on by default).
    pub formula_guard: bool,
    /// Size bound of the text.
    pub max_bytes: usize,
    /// Table and dialect, for SQL `INSERT`.
    pub target: Option<InsertTarget>,
}

impl AdvancedCopy {
    /// Defaults for `format`: formula guard on, bounded by [`MAX_COPY_BYTES`].
    #[must_use]
    pub fn new(format: AdvancedFormat) -> Self {
        Self {
            format,
            formula_guard: true,
            max_bytes: MAX_COPY_BYTES,
            target: None,
        }
    }

    /// With a SQL `INSERT` target.
    #[must_use]
    pub fn with_target(mut self, target: Option<InsertTarget>) -> Self {
        self.target = target;
        self
    }
}

/// Why an advanced copy was refused. Messages name columns, never values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CopyError {
    /// Nothing is selected.
    NothingSelected,
    /// SQL `INSERT` without a table.
    NoTable,
    /// SQL `INSERT` on a column whose values the driver cut.
    CutValues(String),
    /// The SQL generator or the exporter refused a value.
    Refused(String),
}

impl std::error::Error for CopyError {}

impl std::fmt::Display for CopyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NothingSelected => f.write_str("Nothing is selected."),
            Self::NoTable => f.write_str(
                "Copy as SQL INSERT needs the table the rows go to: open the result from the navigator, or give the table name.",
            ),
            Self::CutValues(col) => write!(
                f,
                "Column \"{col}\" holds values cut by the driver at the value size limit: SQL INSERT is not offered for it."
            ),
            Self::Refused(why) => write!(f, "Copy refused: {why}"),
        }
    }
}

/// Copies the selection of `model` (display coordinates, through `rows` and `order`) in
/// `options.format`.
///
/// # Errors
/// [`CopyError`].
pub fn copy_advanced(
    model: &ResultSetModel,
    order: &ColumnOrder,
    rows: &RowView,
    selection: &GridSelection,
    options: &AdvancedCopy,
) -> Result<CopyOutput, CopyError> {
    let display_rows = selection.row_range(rows.len(model.row_count()));
    let cols: Vec<usize> = selection
        .col_range(order.len())
        .filter_map(|d| order.model(d))
        .collect();
    if cols.is_empty() || display_rows.is_empty() {
        return Err(CopyError::NothingSelected);
    }
    let model_rows: Vec<usize> = display_rows.filter_map(|d| rows.model_row(d)).collect();
    let cut_values = cols.iter().any(|&c| model.columns()[c].truncated_values);
    match options.format.export_format() {
        Some(format) => copy_exported(model, &cols, &model_rows, format, options, cut_values),
        None => copy_inserts(model, &cols, &model_rows, options, cut_values),
    }
}

fn copy_exported(
    model: &ResultSetModel,
    cols: &[usize],
    model_rows: &[usize],
    format: ExportFormat,
    options: &AdvancedCopy,
    cut_values: bool,
) -> Result<CopyOutput, CopyError> {
    let mut export = ExportOptions::new(format);
    if matches!(format, ExportFormat::Csv | ExportFormat::Tsv) {
        export.formula_guard = options.formula_guard;
    }
    let schema: SchemaRef = Arc::new(Schema::new(
        cols.iter()
            .map(|&c| model.schema().field(c).clone())
            .collect::<Vec<_>>(),
    ));
    let refused = |e: ninesql_core::transfer::TransferError| CopyError::Refused(e.to_string());
    let mut exporter = Exporter::new(Vec::new(), export, None).map_err(refused)?;
    exporter.start(&schema).map_err(refused)?;
    let mut copied = 0usize;
    let mut capped = false;
    // Rows are gathered by chunks (one Arrow gather per chunk) but written and checked against
    // the cap one row at a time, so a chunk of large values cannot overshoot it by more than
    // one row.
    'chunks: for chunk in model_rows.chunks(CHUNK_ROWS) {
        let batch = gather(model, cols, chunk, &schema).map_err(CopyError::Refused)?;
        for i in 0..batch.num_rows() {
            if exporter.bytes() as usize > options.max_bytes {
                capped = true;
                break 'chunks;
            }
            exporter.write_batch(&batch.slice(i, 1)).map_err(refused)?;
            copied += 1;
        }
    }
    let (bytes, _) = exporter.finish().map_err(refused)?;
    let text = String::from_utf8(bytes).map_err(|_| CopyError::Refused("not UTF-8".into()))?;
    Ok(CopyOutput {
        text,
        rows: copied,
        cols: cols.len(),
        capped,
        cut_values,
    })
}

/// The rows `rows` (model indices) of columns `cols`, as one batch.
fn gather(
    model: &ResultSetModel,
    cols: &[usize],
    rows: &[usize],
    schema: &SchemaRef,
) -> Result<RecordBatch, String> {
    let located: Vec<(usize, usize)> = rows.iter().filter_map(|&r| model.locate(r)).collect();
    let mut arrays: Vec<ArrayRef> = Vec::with_capacity(cols.len());
    for &c in cols {
        let datas: Vec<_> = model
            .batches()
            .iter()
            .map(|b| b.column(c).to_data())
            .collect();
        let mut m = MutableArrayData::new(datas.iter().collect(), true, located.len());
        for &(b, r) in &located {
            m.try_extend(b, r, r + 1).map_err(|e| e.to_string())?;
        }
        arrays.push(make_array(m.freeze()));
    }
    RecordBatch::try_new(schema.clone(), arrays).map_err(|e| e.to_string())
}

fn copy_inserts(
    model: &ResultSetModel,
    cols: &[usize],
    model_rows: &[usize],
    options: &AdvancedCopy,
    cut_values: bool,
) -> Result<CopyOutput, CopyError> {
    let Some(target) = options.target.as_ref().filter(|t| !t.table.is_empty()) else {
        return Err(CopyError::NoTable);
    };
    if let Some(&c) = cols.iter().find(|&&c| model.columns()[c].truncated_values) {
        return Err(CopyError::CutValues(model.columns()[c].name.clone()));
    }
    let mut text = String::new();
    let mut copied = 0usize;
    let mut capped = false;
    let mut values = Vec::with_capacity(cols.len());
    for &row in model_rows {
        if text.len() > options.max_bytes {
            capped = true;
            break;
        }
        let Some((b, r)) = model.locate(row) else {
            continue;
        };
        let batch = &model.batches()[b];
        values.clear();
        for &c in cols {
            let param = cell_value(batch, c, r).map_err(|e| CopyError::Refused(e.to_string()))?;
            let column = DmlColumn::from_field(batch.schema().field(c));
            values.push(ColumnValue::new(column, param));
        }
        let statement = insert_preview(target.dialect.as_ref(), &target.table, &values)
            .map_err(|e| CopyError::Refused(e.to_string()))?;
        text.push_str(&statement);
        text.push_str(";\n");
        copied += 1;
    }
    Ok(CopyOutput {
        text,
        rows: copied,
        cols: cols.len(),
        capped,
        cut_values,
    })
}

/// Parses a table name typed by the user (`table`, `schema.table`, `catalog.schema.table`).
/// Parts are trimmed; a part may not be empty or hold a quote character (names with dots or
/// quotes must come from the navigator, which knows the parts). At most three parts.
///
/// # Errors
/// A user-facing reason.
pub fn parse_table_name(text: &str) -> Result<Vec<String>, &'static str> {
    let parts: Vec<String> = text.split('.').map(|p| p.trim().to_owned()).collect();
    if parts.len() > 3 {
        return Err("Use table, schema.table or catalog.schema.table.");
    }
    if parts.iter().any(String::is_empty) {
        return Err("A part of the table name is empty.");
    }
    if parts
        .iter()
        .any(|p| p.contains(['"', '`', '\'', '[', ']']) || p.chars().any(char::is_control))
    {
        return Err("Names with quotes or control characters must be chosen in the navigator.");
    }
    Ok(parts)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ninesql_core::driver_api::AnsiDialect;
    use ninesql_core::driver_api::arrow_array::{Int64Array, RecordBatch, StringArray};
    use ninesql_core::driver_api::arrow_schema::{DataType, Field, Schema};

    use super::*;
    use crate::model::tests::{batch, truncated_batch};
    use crate::rows::{SortDirection, SortKey};

    fn model(b: Vec<RecordBatch>) -> ResultSetModel {
        let mut m = ResultSetModel::new(0, 0, b[0].schema(), 1000);
        for x in b {
            m.push_batch(x).unwrap();
        }
        m
    }

    fn tricky() -> ResultSetModel {
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("note", DataType::Utf8, true),
        ]));
        let b = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(Int64Array::from(vec![1, 2, 3])),
                Arc::new(StringArray::from(vec![
                    Some("=HYPERLINK(\"x\")"),
                    Some("a|b \"q\"\nline"),
                    None,
                ])),
            ],
        )
        .unwrap();
        model(vec![b])
    }

    fn copy(m: &ResultSetModel, opts: &AdvancedCopy) -> Result<CopyOutput, CopyError> {
        copy_advanced(
            m,
            &ColumnOrder::identity(m.columns().len()),
            &RowView::new(),
            &GridSelection::All,
            opts,
        )
    }

    #[test]
    fn csv_uses_the_export_escaping_and_formula_guard() {
        let m = tricky();
        let out = copy(&m, &AdvancedCopy::new(AdvancedFormat::Csv)).unwrap();
        assert_eq!(
            out.text,
            "id,note\n1,\"'=HYPERLINK(\"\"x\"\")\"\n2,\"a|b \"\"q\"\"\nline\"\n3,\n"
        );
        assert_eq!((out.rows, out.cols), (3, 2));
        let mut raw = AdvancedCopy::new(AdvancedFormat::Csv);
        raw.formula_guard = false;
        assert!(copy(&m, &raw).unwrap().text.contains("\n1,\"=HYPERLINK"));
    }

    #[test]
    fn markdown_and_json_are_escaped() {
        let m = tricky();
        let md = copy(&m, &AdvancedCopy::new(AdvancedFormat::Markdown)).unwrap();
        assert!(md.text.starts_with("| id | note |\n"), "{}", md.text);
        assert!(md.text.contains("a\\|b \\\"q\\\"<br>line"), "{}", md.text);
        let json = copy(&m, &AdvancedCopy::new(AdvancedFormat::Json)).unwrap();
        assert!(json.text.starts_with('['), "{}", json.text);
        assert!(
            json.text.contains(r#""note": "a|b \"q\"\nline""#),
            "{}",
            json.text
        );
        assert!(json.text.contains(r#""note": null"#), "{}", json.text);
        assert!(json.text.trim_end().ends_with(']'));
    }

    #[test]
    fn sql_insert_needs_a_table_and_uses_the_dialect() {
        let m = tricky();
        assert_eq!(
            copy(&m, &AdvancedCopy::new(AdvancedFormat::SqlInsert)),
            Err(CopyError::NoTable)
        );
        let opts = AdvancedCopy::new(AdvancedFormat::SqlInsert).with_target(Some(InsertTarget {
            dialect: Arc::new(AnsiDialect),
            table: vec!["public".into(), "notes".into()],
        }));
        let out = copy(&m, &opts).unwrap();
        let lines: Vec<&str> = out.text.split(";\n").filter(|s| !s.is_empty()).collect();
        assert_eq!(lines.len(), 3);
        assert!(
            lines[0].starts_with("INSERT INTO \"public\".\"notes\" (\"id\", \"note\") VALUES (1, "),
            "{}",
            lines[0]
        );
        assert!(lines[2].ends_with("(3, NULL)"), "{}", lines[2]);
    }

    #[test]
    fn sql_insert_refuses_cut_columns() {
        let m = model(vec![truncated_batch(0, 2)]);
        let opts = AdvancedCopy::new(AdvancedFormat::SqlInsert).with_target(Some(InsertTarget {
            dialect: Arc::new(AnsiDialect),
            table: vec!["t".into()],
        }));
        assert_eq!(copy(&m, &opts), Err(CopyError::CutValues("name".into())));
        // Text formats copy the prefix and flag it.
        let out = copy(&m, &AdvancedCopy::new(AdvancedFormat::Tsv)).unwrap();
        assert!(out.cut_values);
    }

    #[test]
    fn copy_follows_the_sort_and_is_bounded() {
        let m = model(vec![batch(0, 300), batch(300, 300)]);
        let mut rows = RowView::new();
        rows.set_sort(
            Some(SortKey {
                column: 0,
                direction: SortDirection::Descending,
            }),
            &m,
        );
        let sel = GridSelection::Cells {
            anchor: (0, 0),
            focus: (1, 0),
        };
        let out = copy_advanced(
            &m,
            &ColumnOrder::identity(2),
            &rows,
            &sel,
            &AdvancedCopy::new(AdvancedFormat::Csv),
        )
        .unwrap();
        assert_eq!(out.text, "id\n599\n598\n");
        let mut small = AdvancedCopy::new(AdvancedFormat::Csv);
        small.max_bytes = 10;
        let out = copy(&m, &small).unwrap();
        assert!(out.capped);
        // Checked per row, not per chunk: at most one row past the cap.
        assert!((1..=2).contains(&out.rows), "{}", out.rows);
        assert!(out.text.len() <= small.max_bytes + 32, "{}", out.text);
    }

    #[test]
    fn nothing_selected_is_refused() {
        let m = tricky();
        assert_eq!(
            copy_advanced(
                &m,
                &ColumnOrder::identity(2),
                &RowView::new(),
                &GridSelection::None,
                &AdvancedCopy::new(AdvancedFormat::Csv)
            ),
            Err(CopyError::NothingSelected)
        );
    }

    #[test]
    fn table_names_are_parsed_strictly() {
        assert_eq!(
            parse_table_name(" public . users "),
            Ok(vec!["public".to_owned(), "users".to_owned()])
        );
        assert!(parse_table_name("a..b").is_err());
        assert!(parse_table_name("a.b.c.d").is_err());
        assert!(parse_table_name("\"x\".y").is_err());
        assert!(parse_table_name("").is_err());
    }
}
