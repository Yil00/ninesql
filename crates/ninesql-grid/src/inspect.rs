//! Record view and text view (F-DATA-02) and the metadata panel (F-DATA-10), as GPUI-free
//! models over the rows the grid holds.
//!
//! - [`record`]: one row as a form, a line per column (name, type, value), in the grid's column
//!   order. Values use the grid's display rules ([`display_line`]: control and bidirectional
//!   characters made visible), with a longer bound than a cell.
//! - [`text_view`]: the loaded rows (through the client-side sort / filter) as fixed-width plain
//!   text, bounded by [`MAX_TEXT_VIEW_ROWS`] and [`MAX_TEXT_VIEW_BYTES`] (I6), with a note
//!   when cut.
//! - [`column_info`]: name, type, nullability and source of every column.

use crate::format::{DisplayOptions, display_line};
use crate::model::ResultSetModel;
use crate::rows::RowView;
use crate::selection::ColumnOrder;

/// Longest value shown in the record view, in characters.
pub const MAX_RECORD_CHARS: usize = 2000;
/// Rows written at most by the text view.
pub const MAX_TEXT_VIEW_ROWS: usize = 10_000;
/// Bytes written at most by the text view.
pub const MAX_TEXT_VIEW_BYTES: usize = 4 * 1024 * 1024;
/// Widest column of the text view, in characters.
pub const MAX_TEXT_COLUMN_CHARS: usize = 40;

/// One field of the record view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordField {
    /// Result column (model index).
    pub column: usize,
    /// Column name (made visible).
    pub name: String,
    /// Type shown.
    pub type_name: String,
    /// Value (one line, bounded).
    pub text: String,
    /// NULL.
    pub is_null: bool,
    /// The display bound cut the value.
    pub elided: bool,
    /// The driver cut values of this column.
    pub cut: bool,
}

/// Model row `row` as a form, in the display order of `order`. Empty outside the loaded rows.
#[must_use]
pub fn record(model: &ResultSetModel, order: &ColumnOrder, row: usize) -> Vec<RecordField> {
    if row >= model.row_count() {
        return Vec::new();
    }
    let options = DisplayOptions {
        max_chars: MAX_RECORD_CHARS,
        binary_preview_bytes: 64,
        ..DisplayOptions::default()
    };
    (0..order.len())
        .filter_map(|d| order.model(d))
        .filter_map(|c| {
            let meta = model.columns().get(c)?;
            let cell = model.cell(row, c, &options)?;
            Some(RecordField {
                column: c,
                name: display_line(&meta.name, 120).0,
                type_name: meta.type_name().to_owned(),
                text: cell.text,
                is_null: cell.is_null,
                elided: cell.elided,
                cut: meta.truncated_values,
            })
        })
        .collect()
}

/// The text view of a result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextView {
    /// Fixed-width text: a header line, a separator line, one line per row.
    pub text: String,
    /// Rows written.
    pub rows: usize,
    /// Rows the view had (after the client-side filter).
    pub total_rows: usize,
    /// The row or byte bound stopped it early.
    pub capped: bool,
}

fn pad(out: &mut String, s: &str, width: usize, right: bool) {
    let n = s.chars().count();
    let fill = width.saturating_sub(n);
    if right {
        out.extend(std::iter::repeat_n(' ', fill));
        out.push_str(s);
    } else {
        out.push_str(s);
        out.extend(std::iter::repeat_n(' ', fill));
    }
}

/// The loaded rows of `model`, through `rows` and `order`, as fixed-width text. Numbers are
/// right-aligned; values longer than [`MAX_TEXT_COLUMN_CHARS`] end with `…`.
#[must_use]
pub fn text_view(model: &ResultSetModel, order: &ColumnOrder, rows: &RowView) -> TextView {
    let options = DisplayOptions {
        max_chars: MAX_TEXT_COLUMN_CHARS,
        ..DisplayOptions::default()
    };
    let cols: Vec<usize> = (0..order.len()).filter_map(|d| order.model(d)).collect();
    let total_rows = rows.len(model.row_count());
    let shown = total_rows.min(MAX_TEXT_VIEW_ROWS);
    let mut widths: Vec<usize> = cols
        .iter()
        .map(|&c| {
            display_line(&model.columns()[c].name, MAX_TEXT_COLUMN_CHARS)
                .0
                .chars()
                .count()
        })
        .collect();
    for d in 0..shown {
        let Some(r) = rows.model_row(d) else { continue };
        for (i, &c) in cols.iter().enumerate() {
            if let Some(cell) = model.cell(r, c, &options) {
                widths[i] = widths[i].max(cell.text.chars().count());
            }
        }
    }
    let mut text = String::new();
    for (i, &c) in cols.iter().enumerate() {
        if i > 0 {
            text.push_str(" | ");
        }
        pad(
            &mut text,
            &display_line(&model.columns()[c].name, MAX_TEXT_COLUMN_CHARS).0,
            widths[i],
            false,
        );
    }
    text.push('\n');
    for (i, w) in widths.iter().enumerate() {
        if i > 0 {
            text.push_str("-+-");
        }
        text.extend(std::iter::repeat_n('-', *w));
    }
    let mut written = 0;
    let mut capped = shown < total_rows;
    for d in 0..shown {
        if text.len() > MAX_TEXT_VIEW_BYTES {
            capped = true;
            break;
        }
        let Some(r) = rows.model_row(d) else { continue };
        text.push('\n');
        for (i, &c) in cols.iter().enumerate() {
            if i > 0 {
                text.push_str(" | ");
            }
            if let Some(cell) = model.cell(r, c, &options) {
                let right = cell.align == crate::format::Align::Right;
                pad(&mut text, &cell.text, widths[i], right);
            }
        }
        written += 1;
    }
    TextView {
        text,
        rows: written,
        total_rows,
        capped,
    }
}

/// One line of the metadata panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnInfoRow {
    /// 1-based position in the result.
    pub position: usize,
    /// Name (made visible).
    pub name: String,
    /// The engine's type name when the driver gives it.
    pub engine_type: Option<String>,
    /// The Arrow type.
    pub arrow_type: String,
    /// Nullable, as the driver reports it in the result schema.
    pub nullable: bool,
    /// Source table, when known.
    pub source: Option<String>,
    /// The driver cut values of this column.
    pub cut: bool,
}

/// The columns of `model`. `source` is the table the result reads, when the caller knows it
/// (an "Export table" or a table's data view): result sets carry no column origin otherwise,
/// so the panel says "unknown" rather than guess (I8).
#[must_use]
pub fn column_info(model: &ResultSetModel, source: Option<&str>) -> Vec<ColumnInfoRow> {
    model
        .columns()
        .iter()
        .enumerate()
        .map(|(i, meta)| ColumnInfoRow {
            position: i + 1,
            name: display_line(&meta.name, 120).0,
            engine_type: meta.engine_type.clone(),
            arrow_type: meta.arrow_type.clone(),
            nullable: model.schema().field(i).is_nullable(),
            source: source.map(|s| display_line(s, 120).0),
            cut: meta.truncated_values,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::tests::{batch, truncated_batch};
    use crate::rows::{SortDirection, SortKey};

    fn model(b: Vec<ninesql_core::driver_api::arrow_array::RecordBatch>) -> ResultSetModel {
        let mut m = ResultSetModel::new(0, 0, b[0].schema(), 1000);
        for x in b {
            m.push_batch(x).unwrap();
        }
        m
    }

    #[test]
    fn record_follows_column_order() {
        let m = model(vec![batch(0, 5)]);
        let mut order = ColumnOrder::identity(2);
        order.move_column(1, 0);
        let r = record(&m, &order, 3);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].name, "name");
        assert!(r[0].is_null, "id 3 has a NULL name");
        assert_eq!(r[0].text, "NULL");
        assert_eq!(r[1].name, "id");
        assert_eq!(r[1].text, "3");
        assert_eq!(r[1].type_name, "Int64");
        assert!(record(&m, &order, 9).is_empty());
    }

    #[test]
    fn text_view_is_fixed_width_and_follows_the_sort() {
        let m = model(vec![batch(8, 3)]);
        let mut rows = RowView::new();
        rows.set_sort(
            Some(SortKey {
                column: 0,
                direction: SortDirection::Descending,
            }),
            &m,
        );
        let t = text_view(&m, &ColumnOrder::identity(2), &rows);
        assert_eq!(
            t.text,
            "id | name\n---+-----\n10 | NULL\n 9 | n9  \n 8 | n8  "
        );
        assert_eq!(t.rows, 3);
        assert!(!t.capped);
    }

    #[test]
    fn text_view_is_bounded() {
        let m = model(vec![batch(0, MAX_TEXT_VIEW_ROWS + 5)]);
        let t = text_view(&m, &ColumnOrder::identity(2), &RowView::new());
        assert!(t.capped);
        assert_eq!(t.rows, MAX_TEXT_VIEW_ROWS);
        assert_eq!(t.total_rows, MAX_TEXT_VIEW_ROWS + 5);
    }

    #[test]
    fn column_info_reports_types_nullability_and_cut() {
        let m = model(vec![truncated_batch(0, 2)]);
        let info = column_info(&m, Some("public.users"));
        assert_eq!(info.len(), 2);
        assert_eq!(info[0].position, 1);
        assert_eq!(info[0].name, "id");
        assert!(!info[0].nullable);
        assert!(info[1].nullable);
        assert!(info[1].cut);
        assert_eq!(info[1].source.as_deref(), Some("public.users"));
        assert!(column_info(&m, None)[0].source.is_none());
    }
}
