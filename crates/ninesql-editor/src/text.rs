//! Positions in the editor text: byte offsets (what the rope, the splitter and the execution
//! requests use) and 0-based line / column pairs (what the user sees and some engines report).
//!
//! Columns count Unicode scalar values (`char`s), not bytes and not UTF-16 units, so a column is
//! stable whatever the encoding of the engine's message.

use std::ops::Range;

/// A position in a text: byte offset plus 0-based line and column (in `char`s).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TextPosition {
    /// Byte offset, always on a `char` boundary.
    pub offset: usize,
    /// 0-based line.
    pub line: usize,
    /// 0-based column, in `char`s.
    pub column: usize,
}

/// Line starts of a text, for offset ↔ line / column conversions.
#[derive(Clone, Debug)]
pub struct LineIndex<'a> {
    text: &'a str,
    starts: Vec<usize>,
}

impl<'a> LineIndex<'a> {
    /// Indexes `text`. Lines end at `\n`; a `\r` before it belongs to the line.
    #[must_use]
    pub fn new(text: &'a str) -> Self {
        let mut starts = vec![0];
        starts.extend(text.match_indices('\n').map(|(i, _)| i + 1));
        Self { text, starts }
    }

    /// Number of lines (an empty text has one).
    #[must_use]
    pub fn line_count(&self) -> usize {
        self.starts.len()
    }

    /// Byte range of `line` without its `\n`, or `None` past the last line.
    #[must_use]
    pub fn line_range(&self, line: usize) -> Option<Range<usize>> {
        let start = *self.starts.get(line)?;
        let end = self
            .starts
            .get(line + 1)
            .map_or(self.text.len(), |next| next - 1);
        Some(start..end)
    }

    /// The position of byte `offset`, clamped to the text and moved back to a `char` boundary.
    #[must_use]
    pub fn position(&self, offset: usize) -> TextPosition {
        let offset = floor_char_boundary(self.text, offset);
        let line = self.starts.partition_point(|&s| s <= offset) - 1;
        let start = self.starts[line];
        let column = self.text[start..offset].chars().count();
        TextPosition {
            offset,
            line,
            column,
        }
    }

    /// The byte offset of `line` / `column` (0-based, column in `char`s). A column past the end
    /// of the line clamps to the line end; a line past the end clamps to the end of the text.
    #[must_use]
    pub fn offset(&self, line: usize, column: usize) -> usize {
        let Some(range) = self.line_range(line) else {
            return self.text.len();
        };
        let line_text = &self.text[range.clone()];
        let within = line_text
            .char_indices()
            .nth(column)
            .map_or(line_text.len(), |(i, _)| i);
        range.start + within
    }
}

/// The largest `char` boundary of `text` that is `<= offset` (and `<= text.len()`).
#[must_use]
pub fn floor_char_boundary(text: &str, offset: usize) -> usize {
    let mut offset = offset.min(text.len());
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

/// The byte offset of the `n`-th `char` (0-based) of `text`, clamped to its end.
#[must_use]
pub fn char_to_byte(text: &str, n: usize) -> usize {
    text.char_indices().nth(n).map_or(text.len(), |(i, _)| i)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions_round_trip() {
        let text = "select 1;\nselect 'é', 2;\r\n\nend";
        let index = LineIndex::new(text);
        assert_eq!(index.line_count(), 4);
        for (offset, _) in text.char_indices() {
            let p = index.position(offset);
            assert_eq!(index.offset(p.line, p.column), offset, "offset {offset}");
        }
        let p = index.position(text.find('2').unwrap_or(0));
        assert_eq!((p.line, p.column), (1, 12));
    }

    #[test]
    fn clamps_out_of_range() {
        let text = "ab\ncé";
        let index = LineIndex::new(text);
        assert_eq!(index.offset(0, 99), 2);
        assert_eq!(index.offset(9, 0), text.len());
        // Inside the two bytes of `é`: moved back to its start.
        assert_eq!(index.position(5).offset, 4);
        assert_eq!(index.position(99).offset, text.len());
    }

    #[test]
    fn char_offsets() {
        assert_eq!(char_to_byte("aéb", 2), 3);
        assert_eq!(char_to_byte("aéb", 9), 4);
        assert_eq!(floor_char_boundary("é", 1), 0);
    }
}
