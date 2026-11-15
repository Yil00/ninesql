//! Text operations behind editor actions (F-SQL-15): line comment toggle, word under the
//! cursor, next occurrence for multi-cursor selection. Pure functions over the text.

use std::ops::Range;

/// The edit a toggle produces: replace `range` with `text`, then select `selection`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextEdit {
    /// Byte range replaced.
    pub range: Range<usize>,
    /// Replacement.
    pub text: String,
    /// Selection after the edit.
    pub selection: Range<usize>,
}

fn line_start(text: &str, offset: usize) -> usize {
    text[..offset].rfind('\n').map_or(0, |p| p + 1)
}

fn line_end(text: &str, offset: usize) -> usize {
    text[offset..].find('\n').map_or(text.len(), |p| offset + p)
}

/// Toggles `-- ` line comments on the lines touched by `selection`. When every non-blank line
/// is commented, the comments are removed; otherwise `-- ` is added at the smallest
/// indentation. A selection ending at the start of a line does not touch that line.
#[must_use]
pub fn toggle_line_comment(text: &str, selection: Range<usize>) -> Option<TextEdit> {
    let start = line_start(text, selection.start.min(text.len()));
    let mut end_at = selection.end.min(text.len());
    if end_at > selection.start && end_at > start && text[..end_at].ends_with('\n') {
        end_at -= 1;
    }
    let end = line_end(text, end_at.max(start));
    let block = &text[start..end];
    let lines: Vec<&str> = block.split('\n').collect();
    let non_blank: Vec<&&str> = lines.iter().filter(|l| !l.trim().is_empty()).collect();
    if non_blank.is_empty() {
        return None;
    }
    let commented = non_blank.iter().all(|l| l.trim_start().starts_with("--"));
    let new_lines: Vec<String> = if commented {
        lines
            .iter()
            .map(|l| {
                let indent = l.len() - l.trim_start().len();
                let rest = &l[indent..];
                match rest.strip_prefix("-- ").or_else(|| rest.strip_prefix("--")) {
                    Some(r) => format!("{}{r}", &l[..indent]),
                    None => (*l).to_owned(),
                }
            })
            .collect()
    } else {
        let indent = non_blank
            .iter()
            .map(|l| l.len() - l.trim_start().len())
            .min()
            .unwrap_or(0);
        lines
            .iter()
            .map(|l| {
                if l.trim().is_empty() {
                    (*l).to_owned()
                } else {
                    let cut = indent.min(l.len());
                    format!("{}-- {}", &l[..cut], &l[cut..])
                }
            })
            .collect()
    };
    let replacement = new_lines.join("\n");
    let selection = start..start + replacement.len();
    Some(TextEdit {
        range: start..end,
        text: replacement,
        selection,
    })
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The word around `offset` (identifier characters), if any.
#[must_use]
pub fn word_at(text: &str, offset: usize) -> Option<Range<usize>> {
    let offset = crate::text::floor_char_boundary(text, offset);
    let start = text[..offset]
        .char_indices()
        .rev()
        .take_while(|&(_, c)| is_word_char(c))
        .last()
        .map_or(offset, |(i, _)| i);
    let end = text[offset..]
        .char_indices()
        .find(|&(_, c)| !is_word_char(c))
        .map_or(text.len(), |(i, _)| offset + i);
    (start < end).then_some(start..end)
}

/// The next occurrence of `needle` starting at or after `from`, wrapping to the start; never
/// one of `taken`.
#[must_use]
pub fn next_occurrence(
    text: &str,
    needle: &str,
    from: usize,
    taken: &[Range<usize>],
) -> Option<Range<usize>> {
    if needle.is_empty() {
        return None;
    }
    let from = crate::text::floor_char_boundary(text, from);
    let candidates = text[from..]
        .match_indices(needle)
        .map(|(i, _)| from + i)
        .chain(text[..from].match_indices(needle).map(|(i, _)| i));
    candidates
        .map(|i| i..i + needle.len())
        .find(|r| !taken.contains(r))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comments_and_uncomments() {
        let text = "select 1\n  from t\n\nwhere x";
        let Some(edit) = toggle_line_comment(text, 0..12) else {
            panic!("edit")
        };
        assert_eq!(edit.text, "-- select 1\n--   from t");
        assert_eq!(edit.range, 0..17);
        let mut after = text.to_owned();
        after.replace_range(edit.range.clone(), &edit.text);
        let Some(back) = toggle_line_comment(&after, edit.selection.clone()) else {
            panic!("edit")
        };
        let mut again = after.clone();
        again.replace_range(back.range.clone(), &back.text);
        assert_eq!(again, text);
        // Indented block keeps its indentation; blank lines untouched.
        let Some(e) = toggle_line_comment("  a\n\n    b", 0..10) else {
            panic!("edit")
        };
        assert_eq!(e.text, "  -- a\n\n  --   b");
        // Cursor only: its line.
        let Some(e) = toggle_line_comment("a\nb\nc", 2..2) else {
            panic!("edit")
        };
        assert_eq!((e.range, e.text.as_str()), (2..3, "-- b"));
        // Selection ending at a line start does not touch that line.
        let Some(e) = toggle_line_comment("a\nb\nc", 0..2) else {
            panic!("edit")
        };
        assert_eq!(e.text, "-- a");
        assert_eq!(toggle_line_comment("\n\n", 0..2), None);
    }

    #[test]
    fn words_and_occurrences() {
        let text = "select id, user_id from t where id = 1";
        assert_eq!(word_at(text, 8).map(|r| &text[r]), Some("id"));
        assert_eq!(word_at(text, 9), Some(7..9), "right after the word");
        assert_eq!(word_at(text, 6), Some(0..6));
        assert_eq!(word_at("a  b", 2), None);
        let first = 7..9;
        let next = next_occurrence(text, "id", first.end, std::slice::from_ref(&first));
        assert_eq!(next, Some(16..18), "inside user_id: a plain text match");
        let wrapped = next_occurrence(text, "select", 10, &[]);
        assert_eq!(wrapped, Some(0..6));
        let taken = [Range { start: 0, end: 6 }];
        assert_eq!(next_occurrence(text, "select", 10, &taken), None);
    }
}
