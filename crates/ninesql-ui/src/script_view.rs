//! A read-only, virtualized view of a script a confirmation shows (phase 5 UI review M1): the
//! table-editing save confirmation, the DML preview and the CSV import confirmation.
//!
//! Nothing is ever cut: a long line is wrapped, at the width measured from the box and its mono
//! font, onto as many display rows as it needs (every character stays visible, a continuation
//! row is marked), so the `WHERE` clause at the end of a long `UPDATE` is always in the view.
//! Only the rows in sight are built (`uniform_list`), so a save of 100 000 deletes costs a row
//! index, not 100 000 elements (I6). Invisible, line-breaking and bidirectional characters are
//! shown as visible markers, the same rule as the navigator's labels
//! ([`crate::browse::is_invisible`]), so the text cannot hide or reorder a clause (I3). The line
//! and byte counts and the last line ("Ends with") are shown with it, as in the phase 3 run
//! confirmation.

use std::cell::{Cell, Ref, RefCell};
use std::ops::Range;
use std::rc::Rc;

use gpui_kit::component::{ActiveTheme as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::browse::{INVISIBLE_MARKER, SPACE_MARKER, is_invisible, is_odd_space};
use crate::connections::flow::StatementPreview;

/// Rows the box shows before it scrolls.
pub const SCRIPT_BOX_ROWS: usize = 16;
/// Height of one display row.
const ROW_HEIGHT: f32 = 16.;
/// Upper bound of the width of a wide character (East Asian wide, emoji, symbols and the
/// markers, which may come from a fallback font), in ems of the font size. CJK ideographs are
/// 1 em; color emoji fonts advance about 1.25 em. Over-counting is safe, under-counting is not.
pub const WIDE_EM: f32 = 1.3;
/// Columns kept free at the end of each row, so a rounding or a hinting difference cannot push
/// the last character out of the box.
const SPARE_COLUMNS: usize = 1;
/// The fewest columns a row has, whatever the box's width.
pub const MIN_COLUMNS: usize = 8;

/// How wide a display row may be, in columns of the mono font's advance.
///
/// A character takes one column, or [`WrapWidth::wide`] columns when it is wide
/// ([`is_wide`]). The width is measured from the box and the mono font when the box is laid
/// out ([`script_view`]); until then [`WrapWidth::FALLBACK`] keeps a row inside the narrowest
/// box a confirmation dialog has (72 columns of 7.2 px are 518 px).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WrapWidth {
    /// Columns of one row.
    pub columns: usize,
    /// Columns a wide character takes.
    pub wide: usize,
}

impl WrapWidth {
    /// The width used before the box is measured.
    pub const FALLBACK: Self = Self {
        columns: 72,
        wide: 3,
    };

    /// The width that fits a row box `content` pixels wide whose rows start with a line-number
    /// gutter of `gutter_chars` characters and a `gap`, in a mono font of size `font_size` whose
    /// advance is `advance` (all in pixels).
    #[must_use]
    pub fn measured(
        content: f32,
        advance: f32,
        font_size: f32,
        gutter_chars: usize,
        gap: f32,
    ) -> Self {
        let advance = advance.max(1.);
        let wide_px = WIDE_EM * font_size.max(1.);
        // `wide_px / advance` is a few columns at most; the cast cannot truncate.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let wide = ((wide_px / advance).ceil() as usize).max(2);
        let gutter = Self::gutter_px(advance, font_size, gutter_chars);
        let text = (content - gutter - gap).max(0.);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let fit = (text / advance).floor() as usize;
        Self {
            columns: fit.saturating_sub(SPARE_COLUMNS).max(MIN_COLUMNS).max(wide),
            wide,
        }
    }

    /// Upper bound of the gutter's width: the line number, or the continuation marker (which
    /// may come from a fallback font), plus one spare advance.
    #[must_use]
    pub fn gutter_px(advance: f32, font_size: f32, gutter_chars: usize) -> f32 {
        #[allow(clippy::cast_precision_loss)]
        let digits = gutter_chars as f32 * advance;
        digits.max(WIDE_EM * font_size) + advance
    }
}

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

/// The rows of a script wrapped at one width.
#[derive(Debug, Clone)]
struct Layout {
    width: WrapWidth,
    rows: Vec<ScriptRow>,
    /// The row with the most columns.
    widest: usize,
}

/// A script and its display rows, built once per dialog and re-wrapped when the box's width
/// changes.
#[derive(Debug)]
pub struct ScriptRows {
    text: Rc<str>,
    layout: RefCell<Layout>,
    /// The row box's last measured width in pixels and the mono advance it was measured with.
    measured: Cell<Option<(f32, f32)>>,
    preview: StatementPreview,
}

/// Whether `c` may take more than one column in a monospace font: East Asian wide, emoji,
/// regional indicators, symbols and dingbats, arrows and the markers this view draws, which
/// may come from a fallback font. Conservative: over-counting only wraps a row earlier.
fn is_wide(c: char) -> bool {
    matches!(
        c,
        '\u{1100}'..='\u{115F}'
            | '\u{2190}'..='\u{21FF}'
            | '\u{2300}'..='\u{23FF}'
            | '\u{2400}'..='\u{24FF}'
            | '\u{25A0}'..='\u{27BF}'
            | '\u{2B00}'..='\u{2BFF}'
            | '\u{2E80}'..='\u{A4CF}'
            | '\u{A960}'..='\u{A97F}'
            | '\u{AC00}'..='\u{D7FF}'
            | '\u{F900}'..='\u{FAFF}'
            | '\u{FE10}'..='\u{FE19}'
            | '\u{FE30}'..='\u{FE4F}'
            | '\u{FF00}'..='\u{FF60}'
            | '\u{FFE0}'..='\u{FFE6}'
            | '\u{FFFC}'..='\u{FFFD}'
            | '\u{16FE0}'..='\u{18AFF}'
            | '\u{1B000}'..='\u{1B2FF}'
            | '\u{1F000}'..='\u{1FAFF}'
            | '\u{20000}'..='\u{3FFFD}'
    )
}

/// Columns `c` takes once marked ([`mark_invisible`]) at `width`.
fn columns_of(c: char, width: WrapWidth) -> usize {
    let shown = mark_char(c);
    if is_wide(shown) || is_wide(c) {
        width.wide
    } else {
        1
    }
}

/// The display rows of `text` wrapped at `width`, and the index of the widest row.
fn wrap(text: &str, width: WrapWidth) -> (Vec<ScriptRow>, usize) {
    let mut rows = Vec::new();
    let mut widest = (0, 0);
    let mut push = |rows: &mut Vec<ScriptRow>, row: ScriptRow, columns: usize| {
        if columns > widest.1 {
            widest = (rows.len(), columns);
        }
        rows.push(row);
    };
    let mut start = 0;
    for (n, line) in text.split('\n').enumerate() {
        let end = start + line.len();
        let visible_end = if line.ends_with('\r') { end - 1 } else { end };
        let mut row_start = start;
        let mut columns = 0;
        let mut continuation = false;
        for (i, c) in text[start..visible_end].char_indices() {
            let w = columns_of(c, width);
            if columns > 0 && columns + w > width.columns {
                push(
                    &mut rows,
                    ScriptRow {
                        range: (row_start, start + i),
                        line: n + 1,
                        continuation,
                    },
                    columns,
                );
                row_start = start + i;
                columns = 0;
                continuation = true;
            }
            columns += w;
        }
        push(
            &mut rows,
            ScriptRow {
                range: (row_start, visible_end),
                line: n + 1,
                continuation,
            },
            columns,
        );
        start = end + 1;
    }
    (rows, widest.0)
}

impl ScriptRows {
    /// The rows of `text` (line ends `\n` or `\r\n`) at [`WrapWidth::FALLBACK`], until the box
    /// is measured.
    #[must_use]
    pub fn new(text: &str) -> Self {
        Self::with_width(text, WrapWidth::FALLBACK)
    }

    /// The rows of `text` wrapped at `width`.
    #[must_use]
    pub fn with_width(text: &str, width: WrapWidth) -> Self {
        let (rows, widest) = wrap(text, width);
        Self {
            preview: StatementPreview::of(text),
            text: Rc::from(text),
            layout: RefCell::new(Layout {
                width,
                rows,
                widest,
            }),
            measured: Cell::new(None),
        }
    }

    /// Re-wraps the rows at `width`; whether they changed.
    pub fn set_width(&self, width: WrapWidth) -> bool {
        if self.layout.borrow().width == width {
            return false;
        }
        let (rows, widest) = wrap(&self.text, width);
        *self.layout.borrow_mut() = Layout {
            width,
            rows,
            widest,
        };
        true
    }

    /// The width the rows are wrapped at.
    #[must_use]
    pub fn width(&self) -> WrapWidth {
        self.layout.borrow().width
    }

    /// The row box's last measured width and the mono advance, in pixels; `None` before the
    /// box was laid out.
    #[must_use]
    pub fn measured(&self) -> Option<(f32, f32)> {
        self.measured.get()
    }

    /// Display rows.
    #[must_use]
    pub fn rows(&self) -> Ref<'_, [ScriptRow]> {
        Ref::map(self.layout.borrow(), |l| l.rows.as_slice())
    }

    /// Number of display rows.
    #[must_use]
    pub fn row_count(&self) -> usize {
        self.layout.borrow().rows.len()
    }

    /// The row with the most columns.
    #[must_use]
    pub fn widest_row(&self) -> usize {
        self.layout.borrow().widest
    }

    /// Columns display row `ix` takes at the current width; `None` past the end.
    #[must_use]
    pub fn row_columns(&self, ix: usize) -> Option<usize> {
        let layout = self.layout.borrow();
        let row = layout.rows.get(ix)?;
        Some(
            self.text[row.range.0..row.range.1]
                .chars()
                .map(|c| columns_of(c, layout.width))
                .sum(),
        )
    }

    /// Characters of the gutter (the widest line number).
    #[must_use]
    pub fn gutter_chars(&self) -> usize {
        let layout = self.layout.borrow();
        format!("{}", layout.rows.last().map_or(1, |r| r.line)).len()
    }

    /// The text of display row `ix`, invisible characters marked; `None` past the end.
    #[must_use]
    pub fn row_text(&self, ix: usize) -> Option<String> {
        let row = *self.layout.borrow().rows.get(ix)?;
        Some(mark_invisible(&self.text[row.range.0..row.range.1]))
    }

    /// Every display row, joined with `\n` (tests: proves nothing is cut).
    #[must_use]
    pub fn shown_text(&self) -> String {
        (0..self.row_count())
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
        (self.row_count() > SCRIPT_BOX_ROWS || self.preview.needs_last_line())
            .then(|| mark_invisible(&self.preview.last_line))
    }

    /// Measures the row box (`bounds`, laid out in the box's mono text style) and re-wraps the
    /// rows if the width changed, asking for another frame to show them. Keeps the current
    /// width when the font cannot be measured.
    fn measure(&self, bounds: Bounds<Pixels>, window: &mut Window) {
        let style = window.text_style();
        let rem = window.rem_size();
        let font_size = style.font_size.to_pixels(rem);
        let text_system = window.text_system();
        let font_id = text_system.resolve_font(&style.font());
        let advance = ['m', 'W', '0']
            .into_iter()
            .filter_map(|c| text_system.advance(font_id, font_size, c).ok())
            .map(|a| f32::from(a.width))
            .fold(0f32, f32::max);
        if advance <= 0. || bounds.size.width <= px(0.) {
            return;
        }
        let content = f32::from(bounds.size.width);
        self.measured.set(Some((content, advance)));
        let gap = f32::from(rems(0.5).to_pixels(rem));
        let width = WrapWidth::measured(
            content,
            advance,
            f32::from(font_size),
            self.gutter_chars(),
            gap,
        );
        if self.set_width(width) {
            // Drawing now: the new rows are built in the next frame.
            window.on_next_frame(|window, _| window.refresh());
        }
    }
}

/// The marker [`mark_invisible`] shows for `c`, or `c`.
fn mark_char(c: char) -> char {
    if c == '\t' {
        '\u{2192}'
    } else if is_invisible(c) {
        INVISIBLE_MARKER
    } else if is_odd_space(c) {
        SPACE_MARKER
    } else {
        c
    }
}

/// `text` with invisible characters ([`is_invisible`]) shown as [`INVISIBLE_MARKER`] and odd
/// spaces ([`is_odd_space`]) as [`SPACE_MARKER`]; a tab is shown as `→` and keeps its place.
/// Never cut.
#[must_use]
pub fn mark_invisible(text: &str) -> String {
    text.chars().map(mark_char).collect()
}

/// The script box: size line, the virtualized rows with their line numbers, and the "Ends with"
/// line when the end can be out of sight.
///
/// The rows are wrapped to the box: a zero-size canvas measures the row box and the mono
/// font's advance at every frame and re-wraps when the width changes ([`ScriptRows::measure`]),
/// so a narrower dialog wraps sooner instead of clipping the end of a row (phase 5 re-review
/// M-A, I3). As a last resort, the list scrolls horizontally to its widest row, so a glyph
/// wider than counted never makes text unreachable.
pub fn script_view(id: &'static str, script: Rc<ScriptRows>, cx: &App) -> AnyElement {
    let mono = cx.theme().mono_font_family.clone();
    let muted = cx.theme().muted_foreground;
    let count = script.row_count();
    let gutter = script.gutter_chars();
    let visible = count.clamp(1, SCRIPT_BOX_ROWS);
    let rows = script.clone();
    let row_mono = mono.clone();
    let list = uniform_list(id, count, move |range: Range<usize>, _window, _cx| {
        range
            .filter_map(|ix| {
                let row = *rows.rows().get(ix)?;
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
                        .child(div().flex_none().child(text)),
                )
            })
            .collect()
    })
    .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
    .with_width_from_item(Some(script.widest_row()))
    .w_full()
    .h(px(visible as f32 * ROW_HEIGHT));
    let measured = script.clone();
    let rows_box = div()
        .relative()
        .w_full()
        .font_family(mono.clone())
        .text_xs()
        .child(list)
        .child(
            canvas(
                move |bounds, window, _| measured.measure(bounds, window),
                |_, (), _, _| {},
            )
            .absolute()
            .top_0()
            .left_0()
            .size_full(),
        );
    v_flex()
        .gap_1()
        .child(
            div()
                .id(SharedString::from(format!("{id}-size")))
                .text_xs()
                .text_color(muted)
                .child(script.size_text()),
        )
        .child(div().border_1().border_color(muted).p_1().child(rows_box))
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
    use super::{ScriptRows, WIDE_EM, WrapWidth, is_wide};

    /// The fixed width the view used before the re-review (96 columns, wide characters two).
    const OLD: WrapWidth = WrapWidth {
        columns: 96,
        wide: 2,
    };

    #[test]
    fn long_lines_wrap_and_nothing_is_cut() {
        let long = format!(
            "UPDATE t SET v = '{}' WHERE id = 42;",
            "x".repeat(OLD.columns * 3)
        );
        let s = ScriptRows::with_width(&long, OLD);
        assert!(s.rows().len() >= 4, "{:?}", s.rows());
        assert!(s.rows()[1..].iter().all(|r| r.continuation && r.line == 1));
        // Concatenating the rows gives the line back, WHERE clause included.
        let joined: String = (0..s.rows().len())
            .map(|i| s.row_text(i).unwrap_or_default())
            .collect();
        assert_eq!(joined, long);
        assert!(
            s.row_text(s.rows().len() - 1)
                .unwrap_or_default()
                .ends_with("WHERE id = 42;")
        );
        // Every row fits the box.
        assert!(
            (0..s.rows().len())
                .all(|i| s.row_text(i).unwrap_or_default().chars().count() <= OLD.columns)
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
            s.row_text(99_999).unwrap_or_default(),
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
        assert_eq!(
            s.row_text(0).unwrap_or_default(),
            "UPDATE t SET a = 'x\u{FFFD}y'"
        );
        assert_eq!(
            s.row_text(1).unwrap_or_default(),
            "WHERE\u{2423}id\u{2192}= 1;\u{FFFD}"
        );
    }

    #[test]
    fn wide_characters_count_as_wide() {
        let s = ScriptRows::with_width(&"表".repeat(OLD.columns), OLD);
        assert_eq!(s.rows().len(), 2);
        assert_eq!(
            s.row_text(0).unwrap_or_default().chars().count(),
            OLD.columns / 2
        );
        // Regional indicators, symbols and dingbats, arrows and the other blocks a font may
        // draw wider than one advance (phase 5 re-review M-A).
        for c in [
            '\u{1F1E6}',
            '\u{1F1EB}',
            '\u{1F1FF}',
            '\u{2600}',
            '\u{2614}',
            '\u{2705}',
            '\u{2764}',
            '\u{27BF}',
            '\u{2B00}',
            '\u{2B50}',
            '\u{2BFF}',
            '\u{231A}',
            '\u{21AA}',
            '\u{FFFD}',
            '\u{2423}',
            '\u{1F600}',
            '\u{1FAFF}',
            '\u{3000}',
            '\u{AC00}',
        ] {
            assert!(is_wide(c), "{c:?}");
        }
        for c in ['a', 'Z', '0', ' ', '\u{00E9}', '\u{0416}', '\u{03A9}'] {
            assert!(!is_wide(c), "{c:?}");
        }
        // A marker stands for an invisible character: it is counted as wide.
        let marked = ScriptRows::with_width(&"\u{200B}".repeat(10), OLD);
        assert_eq!(marked.row_columns(0), Some(20));
    }

    /// The worst-case width in pixels of display row `ix` with its gutter: one advance per
    /// narrow character, `WIDE_EM` of the font size per wide one (wider than any CJK
    /// ideograph or color emoji).
    fn worst_row_px(s: &ScriptRows, ix: usize, advance: f32, font_size: f32, gap: f32) -> f32 {
        let text = s.row_text(ix).unwrap_or_default();
        let body: f32 = text
            .chars()
            .map(|c| {
                if is_wide(c) {
                    (WIDE_EM * font_size).max(advance)
                } else {
                    advance
                }
            })
            .sum();
        WrapWidth::gutter_px(advance, font_size, s.gutter_chars()) + gap + body
    }

    fn sample_script() -> String {
        let mut script = String::new();
        script.push_str(&format!(
            "UPDATE \"public\".\"orders\" SET \"note\" = '{}' WHERE \"id\" = 4;\n",
            "long text ".repeat(30)
        ));
        script.push_str(&format!("INSERT INTO t VALUES ('{}');\n", "表".repeat(200)));
        script.push_str(&format!(
            "UPDATE t SET flag = '{}' WHERE id = 7;\n",
            "\u{1F1EB}\u{1F1F7}\u{2600}\u{2B50}\u{1F600}".repeat(40)
        ));
        script.push_str(&format!(
            "DELETE FROM t WHERE a = '{}\t\u{200B}\u{00A0}' AND id = 9;",
            "\u{2192}x".repeat(120)
        ));
        script
    }

    /// The widest row fits the row box for the confirmation dialogs' width (720 px, and
    /// narrower when the window shrinks), with the mono advance of the usual fonts (0.5 em to
    /// 0.65 em at `text_xs`, 12 px): the end of a row is never clipped (phase 5 re-review M-A).
    #[test]
    fn the_widest_row_fits_the_dialog_content_width() {
        let font_size = 12.;
        let gap = 8.;
        let script = sample_script();
        for advance in [6.0, 7.2, 7.8] {
            // 720 px dialogs leave about 627 to 658 px to the rows; a narrow window less.
            for content in [200., 360., 480., 627., 640., 658., 700.] {
                let width = WrapWidth::measured(content, advance, font_size, 1, gap);
                let s = ScriptRows::with_width(&script, width);
                for ix in 0..s.row_count() {
                    let px = worst_row_px(&s, ix, advance, font_size, gap);
                    assert!(
                        px <= content,
                        "row {ix} needs {px} px of {content} px at advance {advance}: {:?}",
                        s.row_text(ix)
                    );
                }
                let widest = s.widest_row();
                assert!(
                    (0..s.row_count()).all(|i| s.row_columns(i) <= s.row_columns(widest)),
                    "widest row index"
                );
                // Nothing is cut, and each row is only a slice of its line.
                assert_eq!(
                    s.shown_text().replace('\n', ""),
                    script
                        .split('\n')
                        .map(super::mark_invisible)
                        .collect::<String>()
                );
            }
            // Before the box is measured, the fallback fits the narrowest 720 px dialog.
            let s = ScriptRows::new(&script);
            for ix in 0..s.row_count() {
                assert!(worst_row_px(&s, ix, advance, font_size, gap) <= 627.);
            }
        }
    }

    /// The reviewed example: a long `UPDATE … WHERE id = 4` keeps its key in sight.
    #[test]
    fn the_key_of_a_long_update_stays_in_sight() {
        let update = format!(
            "UPDATE \"orders\" SET \"note\" = '{}' WHERE id = 4",
            "n".repeat(70)
        );
        let s = ScriptRows::with_width(&update, WrapWidth::measured(627., 7.2, 12., 1, 8.));
        assert!(s.width().columns < OLD.columns, "{:?}", s.width());
        let last = s.row_count() - 1;
        assert!(
            s.row_text(last)
                .unwrap_or_default()
                .ends_with("WHERE id = 4")
        );
        assert!((0..s.row_count()).all(|i| s.row_columns(i) <= Some(s.width().columns)));
    }

    #[test]
    fn rewraps_only_when_the_width_changes() {
        let s = ScriptRows::new(&"x".repeat(200));
        assert_eq!(s.width(), WrapWidth::FALLBACK);
        assert_eq!(s.row_count(), 3);
        assert!(!s.set_width(WrapWidth::FALLBACK));
        assert!(s.set_width(WrapWidth {
            columns: 100,
            wide: 2
        }));
        assert_eq!(s.row_count(), 2);
        // A width too small for one wide character still makes progress.
        let tiny = ScriptRows::with_width(
            "表表",
            WrapWidth {
                columns: 1,
                wide: 3,
            },
        );
        assert_eq!(tiny.row_count(), 2);
        assert!(WrapWidth::measured(10., 7.2, 12., 1, 8.).columns >= super::MIN_COLUMNS);
    }
}
