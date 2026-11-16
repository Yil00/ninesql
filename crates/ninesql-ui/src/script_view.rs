//! A read-only, virtualized view of a script a confirmation shows (phase 5 UI review M1): the
//! table-editing save confirmation, the DML preview and the CSV import confirmation.
//!
//! Nothing is ever cut: a long line is wrapped onto as many display rows as it needs (every
//! character stays visible, a continuation row is marked), so the `WHERE` clause at the end of a
//! long `UPDATE` is always in the view. Only the rows in sight are built (`uniform_list`), so a
//! save of 100 000 deletes costs a row index, not 100 000 elements (I6). Invisible, line-breaking
//! and bidirectional characters are shown as visible markers, the same rule as the navigator's
//! labels ([`crate::browse::is_invisible`]), so the text cannot hide or reorder a clause (I3).
//! The line and byte counts and the last line ("Ends with") are shown with it, as in the phase 3
//! run confirmation.

use std::ops::Range;
use std::rc::Rc;

use gpui_kit::component::{ActiveTheme as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::browse::{INVISIBLE_MARKER, SPACE_MARKER, is_invisible, is_odd_space};
use crate::connections::flow::StatementPreview;

/// Display columns of one row: longer lines continue on the next row (wide characters count
/// two).
pub const SCRIPT_ROW_COLUMNS: usize = 96;
/// Rows the box shows before it scrolls.
pub const SCRIPT_BOX_ROWS: usize = 16;
/// Height of one display row.
const ROW_HEIGHT: f32 = 16.;

/// One display row: a slice of one source line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScriptRow {
    /// Bytes of the script shown on this row (no line break).
    pub range: (usize, usize),
    /// 1-based source line.
    pub line: usize,
    /// The row continues the line of the row above.
    pub continuation: bool,
}

/// A script and its display rows, built once per dialog.
#[derive(Debug, Clone)]
pub struct ScriptRows {
    text: Rc<str>,
    rows: Vec<ScriptRow>,
    preview: StatementPreview,
}

/// Whether `c` takes two columns in a monospace font (East Asian wide and emoji ranges; an
/// approximation, enough to keep a row inside the box).
fn is_wide(c: char) -> bool {
    matches!(
        c,
        '\u{1100}'..='\u{115F}'
            | '\u{2E80}'..='\u{A4CF}'
            | '\u{AC00}'..='\u{D7A3}'
            | '\u{F900}'..='\u{FAFF}'
            | '\u{FE30}'..='\u{FE4F}'
            | '\u{FF00}'..='\u{FF60}'
            | '\u{FFE0}'..='\u{FFE6}'
            | '\u{1F300}'..='\u{1FAFF}'
            | '\u{20000}'..='\u{3FFFD}'
    )
}

impl ScriptRows {
    /// The rows of `text` (line ends `\n` or `\r\n`).
    #[must_use]
    pub fn new(text: &str) -> Self {
        let mut rows = Vec::new();
        let mut start = 0;
        for (n, line) in text.split('\n').enumerate() {
            let end = start + line.len();
            let visible_end = if line.ends_with('\r') { end - 1 } else { end };
            let mut row_start = start;
            let mut columns = 0;
            let mut continuation = false;
            for (i, c) in text[start..visible_end].char_indices() {
                let w = if is_wide(c) { 2 } else { 1 };
                if columns + w > SCRIPT_ROW_COLUMNS {
                    rows.push(ScriptRow {
                        range: (row_start, start + i),
                        line: n + 1,
                        continuation,
                    });
                    row_start = start + i;
                    columns = 0;
                    continuation = true;
                }
                columns += w;
            }
            rows.push(ScriptRow {
                range: (row_start, visible_end),
                line: n + 1,
                continuation,
            });
            start = end + 1;
        }
        Self {
            preview: StatementPreview::of(text),
            text: Rc::from(text),
            rows,
        }
    }

    /// Display rows.
    #[must_use]
    pub fn rows(&self) -> &[ScriptRow] {
        &self.rows
    }

    /// The text of display row `ix`, invisible characters marked; `None` past the end.
    #[must_use]
    pub fn row_text(&self, ix: usize) -> Option<String> {
        let row = self.rows.get(ix)?;
        Some(mark_invisible(&self.text[row.range.0..row.range.1]))
    }

    /// Every display row, joined with `\n` (tests: proves nothing is cut).
    #[must_use]
    pub fn shown_text(&self) -> String {
        (0..self.rows.len())
            .filter_map(|i| self.row_text(i))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// `12 lines · 340 bytes`.
    #[must_use]
    pub fn size_text(&self) -> String {
        self.preview.size_text()
    }

    /// The end of the last non-blank line when the script may scroll out of sight.
    #[must_use]
    pub fn ends_with(&self) -> Option<String> {
        (self.rows.len() > SCRIPT_BOX_ROWS || self.preview.needs_last_line())
            .then(|| mark_invisible(&self.preview.last_line))
    }
}

/// `text` with invisible characters ([`is_invisible`]) shown as [`INVISIBLE_MARKER`] and odd
/// spaces ([`is_odd_space`]) as [`SPACE_MARKER`]; a tab is shown as `→` and keeps its place.
/// Never cut.
#[must_use]
pub fn mark_invisible(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c == '\t' {
                '\u{2192}'
            } else if is_invisible(c) {
                INVISIBLE_MARKER
            } else if is_odd_space(c) {
                SPACE_MARKER
            } else {
                c
            }
        })
        .collect()
}

/// The script box: size line, the virtualized rows with their line numbers, and the "Ends with"
/// line when the end can be out of sight.
pub fn script_view(id: &'static str, script: Rc<ScriptRows>, cx: &App) -> AnyElement {
    let mono = cx.theme().mono_font_family.clone();
    let muted = cx.theme().muted_foreground;
    let count = script.rows.len();
    let gutter = format!("{}", script.rows.last().map_or(1, |r| r.line)).len();
    let visible = count.clamp(1, SCRIPT_BOX_ROWS);
    let rows = script.clone();
    let row_mono = mono.clone();
    let list = uniform_list(id, count, move |range: Range<usize>, _window, _cx| {
        range
            .filter_map(|ix| {
                let row = rows.rows.get(ix)?;
                let text = rows.row_text(ix)?;
                let number = if row.continuation {
                    format!("{:>gutter$}", "\u{21AA}")
                } else {
                    format!("{:>gutter$}", row.line)
                };
                Some(
                    h_flex()
                        .id(("script-row", ix))
                        .h(px(ROW_HEIGHT))
                        .gap_2()
                        .font_family(row_mono.clone())
                        .text_xs()
                        .whitespace_nowrap()
                        .child(div().flex_none().text_color(muted).child(number))
                        .child(div().child(text)),
                )
            })
            .collect()
    })
    .h(px(visible as f32 * ROW_HEIGHT));
    v_flex()
        .gap_1()
        .child(
            div()
                .id(SharedString::from(format!("{id}-size")))
                .text_xs()
                .text_color(muted)
                .child(script.size_text()),
        )
        .child(div().border_1().border_color(muted).p_1().child(list))
        .when_some(script.ends_with(), |d, last| {
            d.child(
                div()
                    .id(SharedString::from(format!("{id}-last-line")))
                    .text_xs()
                    .child(div().text_color(muted).child("Ends with:"))
                    .child(div().font_family(mono).child(last)),
            )
        })
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::{SCRIPT_ROW_COLUMNS, ScriptRows};

    #[test]
    fn long_lines_wrap_and_nothing_is_cut() {
        let long = format!(
            "UPDATE t SET v = '{}' WHERE id = 42;",
            "x".repeat(SCRIPT_ROW_COLUMNS * 3)
        );
        let s = ScriptRows::new(&long);
        assert!(s.rows().len() >= 4, "{:?}", s.rows());
        assert!(s.rows()[1..].iter().all(|r| r.continuation && r.line == 1));
        // Concatenating the rows gives the line back, WHERE clause included.
        let joined: String = (0..s.rows().len())
            .map(|i| s.row_text(i).unwrap())
            .collect();
        assert_eq!(joined, long);
        assert!(
            s.row_text(s.rows().len() - 1)
                .unwrap()
                .ends_with("WHERE id = 42;")
        );
        // Every row fits the box.
        assert!(
            (0..s.rows().len())
                .all(|i| s.row_text(i).unwrap().chars().count() <= SCRIPT_ROW_COLUMNS)
        );
    }

    #[test]
    fn lines_count_and_the_end_is_shown() {
        let script: String = (0..100_000)
            .map(|i| format!("DELETE FROM t WHERE id = {i};\n"))
            .collect();
        let s = ScriptRows::new(&script);
        // One row per line (and the empty line after the last line end).
        assert_eq!(s.rows().len(), 100_001);
        assert_eq!(s.rows()[99_999].line, 100_000);
        assert_eq!(
            s.row_text(99_999).unwrap(),
            "DELETE FROM t WHERE id = 99999;"
        );
        assert!(
            s.size_text().starts_with("100001 lines"),
            "{}",
            s.size_text()
        );
        assert_eq!(
            s.ends_with().as_deref(),
            Some("DELETE FROM t WHERE id = 99999;")
        );
        // A short script does not repeat its end.
        assert_eq!(
            ScriptRows::new("DELETE FROM t WHERE id = 1;").ends_with(),
            None
        );
    }

    #[test]
    fn invisible_characters_are_marked_and_crlf_dropped() {
        let s = ScriptRows::new("UPDATE t SET a = 'x\u{202E}y'\r\nWHERE\u{00A0}id\t= 1;\u{200B}");
        assert_eq!(s.rows().len(), 2);
        assert_eq!(s.row_text(0).unwrap(), "UPDATE t SET a = 'x\u{FFFD}y'");
        assert_eq!(
            s.row_text(1).unwrap(),
            "WHERE\u{2423}id\u{2192}= 1;\u{FFFD}"
        );
    }

    #[test]
    fn wide_characters_count_twice() {
        let s = ScriptRows::new(&"表".repeat(SCRIPT_ROW_COLUMNS));
        assert_eq!(s.rows().len(), 2);
        assert_eq!(
            s.row_text(0).unwrap().chars().count(),
            SCRIPT_ROW_COLUMNS / 2
        );
    }
}
