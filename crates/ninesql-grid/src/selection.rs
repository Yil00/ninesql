//! Rectangular selection of cells, rows or columns, in **display** coordinates (column positions
//! after reordering; [`ColumnOrder`] maps them back to result columns).

use std::ops::Range;

/// What is selected. Ranges are inclusive between anchor and focus, in display coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GridSelection {
    /// Nothing.
    #[default]
    None,
    /// A rectangle of cells: `(row, column)`.
    Cells {
        /// Where the selection started.
        anchor: (usize, usize),
        /// The moving corner.
        focus: (usize, usize),
    },
    /// Whole rows.
    Rows {
        /// Where the selection started.
        anchor: usize,
        /// The moving end.
        focus: usize,
    },
    /// Whole columns.
    Columns {
        /// Where the selection started.
        anchor: usize,
        /// The moving end.
        focus: usize,
    },
    /// Every loaded row and column.
    All,
}

fn span(a: usize, b: usize, len: usize) -> Range<usize> {
    if len == 0 {
        return 0..0;
    }
    let lo = a.min(b).min(len - 1);
    let hi = a.max(b).min(len - 1);
    lo..hi + 1
}

fn step(v: usize, delta: isize, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    v.saturating_add_signed(delta).min(len - 1)
}

impl GridSelection {
    /// A single cell.
    #[must_use]
    pub fn cell(row: usize, col: usize) -> Self {
        Self::Cells {
            anchor: (row, col),
            focus: (row, col),
        }
    }

    /// Selected rows, clamped to `rows`.
    #[must_use]
    pub fn row_range(&self, rows: usize) -> Range<usize> {
        match *self {
            Self::None => 0..0,
            Self::Cells { anchor, focus } => span(anchor.0, focus.0, rows),
            Self::Rows { anchor, focus } => span(anchor, focus, rows),
            Self::Columns { .. } | Self::All => 0..rows,
        }
    }

    /// Selected display columns, clamped to `cols`.
    #[must_use]
    pub fn col_range(&self, cols: usize) -> Range<usize> {
        match *self {
            Self::None => 0..0,
            Self::Cells { anchor, focus } => span(anchor.1, focus.1, cols),
            Self::Columns { anchor, focus } => span(anchor, focus, cols),
            Self::Rows { .. } | Self::All => 0..cols,
        }
    }

    /// Number of selected cells.
    #[must_use]
    pub fn cell_count(&self, rows: usize, cols: usize) -> usize {
        self.row_range(rows).len() * self.col_range(cols).len()
    }

    /// The moving corner, as a cell, if any.
    #[must_use]
    pub fn focus(&self) -> Option<(usize, usize)> {
        match *self {
            Self::Cells { focus, .. } => Some(focus),
            Self::Rows { focus, .. } => Some((focus, 0)),
            Self::Columns { focus, .. } => Some((0, focus)),
            Self::None | Self::All => None,
        }
    }

    /// The display column the selection points at (for "auto-size column").
    #[must_use]
    pub fn focus_col(&self) -> Option<usize> {
        match *self {
            Self::Cells { focus, .. } => Some(focus.1),
            Self::Columns { focus, .. } => Some(focus),
            _ => None,
        }
    }

    /// Moves the focus by `(rows, cols)` keeping the anchor (shift + arrow). A row selection
    /// extends by rows, a column selection by columns; nothing selected starts at the first cell.
    pub fn extend(&mut self, d_row: isize, d_col: isize, rows: usize, cols: usize) {
        if rows == 0 || cols == 0 {
            return;
        }
        *self = match *self {
            Self::None | Self::All => Self::cell(0, 0),
            Self::Cells { anchor, focus } => Self::Cells {
                anchor,
                focus: (step(focus.0, d_row, rows), step(focus.1, d_col, cols)),
            },
            Self::Rows { anchor, focus } => Self::Rows {
                anchor,
                focus: step(focus, d_row, rows),
            },
            Self::Columns { anchor, focus } => Self::Columns {
                anchor,
                focus: step(focus, d_col, cols),
            },
        };
    }
}

/// Display order of the columns: `order[display] = result column`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnOrder(Vec<usize>);

impl ColumnOrder {
    /// The identity order of `n` columns.
    #[must_use]
    pub fn identity(n: usize) -> Self {
        Self((0..n).collect())
    }

    /// Result column at display position `display`.
    #[must_use]
    pub fn model(&self, display: usize) -> Option<usize> {
        self.0.get(display).copied()
    }

    /// Every result column, in display order.
    #[must_use]
    pub fn as_slice(&self) -> &[usize] {
        &self.0
    }

    /// Number of columns.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// No column.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Moves the column at display position `from` to `to` (as `Vec::remove` + `insert`).
    pub fn move_column(&mut self, from: usize, to: usize) {
        if from < self.0.len() && to < self.0.len() && from != to {
            let c = self.0.remove(from);
            self.0.insert(to, c);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_clamp_and_normalize() {
        let s = GridSelection::Cells {
            anchor: (5, 3),
            focus: (2, 1),
        };
        assert_eq!(s.row_range(10), 2..6);
        assert_eq!(s.col_range(10), 1..4);
        assert_eq!(s.row_range(4), 2..4);
        assert_eq!(s.cell_count(10, 10), 12);
        assert_eq!(GridSelection::All.row_range(7), 0..7);
        assert_eq!(
            GridSelection::Rows {
                anchor: 1,
                focus: 1
            }
            .col_range(3),
            0..3
        );
        assert_eq!(
            GridSelection::Columns {
                anchor: 2,
                focus: 0
            }
            .col_range(3),
            0..3
        );
        assert_eq!(GridSelection::None.cell_count(5, 5), 0);
        assert_eq!(GridSelection::All.row_range(0), 0..0);
    }

    #[test]
    fn extend_keeps_anchor() {
        let mut s = GridSelection::cell(1, 1);
        s.extend(1, 0, 10, 4);
        s.extend(1, 0, 10, 4);
        s.extend(0, 5, 10, 4);
        assert_eq!(
            s,
            GridSelection::Cells {
                anchor: (1, 1),
                focus: (3, 3)
            }
        );
        s.extend(-10, -10, 10, 4);
        assert_eq!(s.row_range(10), 0..2);
        let mut r = GridSelection::Rows {
            anchor: 4,
            focus: 4,
        };
        r.extend(-2, 1, 10, 4);
        assert_eq!(
            r,
            GridSelection::Rows {
                anchor: 4,
                focus: 2
            }
        );
        let mut n = GridSelection::None;
        n.extend(1, 1, 0, 0);
        assert_eq!(n, GridSelection::None);
        n.extend(1, 1, 3, 3);
        assert_eq!(n, GridSelection::cell(0, 0));
    }

    #[test]
    fn column_order_moves() {
        let mut o = ColumnOrder::identity(4);
        o.move_column(0, 3);
        assert_eq!(o.as_slice(), &[1, 2, 3, 0]);
        o.move_column(3, 1);
        assert_eq!(o.as_slice(), &[1, 0, 2, 3]);
        o.move_column(9, 0);
        assert_eq!(o.as_slice(), &[1, 0, 2, 3]);
        assert_eq!(o.model(1), Some(0));
        assert_eq!(o.model(4), None);
    }
}
