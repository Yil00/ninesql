//! Text hygiene on the editor's commit path (P1-A follow-up "IME"): typed text, IME commits and
//! pastes lose their control characters, and bidirectional controls are reported so the editor
//! can show them (they are invisible in rendered text and can make a statement read differently
//! from what runs, the "Trojan Source" class of attacks).
//!
//! Bidirectional controls are **kept** in the text (a string literal may need one) but always
//! flagged; control characters are removed because no SQL needs them raw.

use std::borrow::Cow;
use std::ops::Range;

/// Whether `c` is removed on the commit path: C0 controls except tab and line feed, DEL and C1
/// controls. Carriage returns are not in this set: [`sanitize`] turns them into line feeds.
#[must_use]
pub fn is_stripped_control(c: char) -> bool {
    matches!(c, '\u{0}'..='\u{8}' | '\u{b}' | '\u{c}' | '\u{e}'..='\u{1f}' | '\u{7f}'..='\u{9f}')
}

/// The name of a bidirectional control character, or `None` for any other character.
#[must_use]
pub fn bidi_control_name(c: char) -> Option<&'static str> {
    Some(match c {
        '\u{061c}' => "ARABIC LETTER MARK",
        '\u{200e}' => "LEFT-TO-RIGHT MARK",
        '\u{200f}' => "RIGHT-TO-LEFT MARK",
        '\u{202a}' => "LEFT-TO-RIGHT EMBEDDING",
        '\u{202b}' => "RIGHT-TO-LEFT EMBEDDING",
        '\u{202c}' => "POP DIRECTIONAL FORMATTING",
        '\u{202d}' => "LEFT-TO-RIGHT OVERRIDE",
        '\u{202e}' => "RIGHT-TO-LEFT OVERRIDE",
        '\u{2066}' => "LEFT-TO-RIGHT ISOLATE",
        '\u{2067}' => "RIGHT-TO-LEFT ISOLATE",
        '\u{2068}' => "FIRST STRONG ISOLATE",
        '\u{2069}' => "POP DIRECTIONAL ISOLATE",
        _ => return None,
    })
}

/// Whether `text` would change under [`sanitize`].
#[must_use]
pub fn needs_sanitizing(text: &str) -> bool {
    text.chars()
        .any(|c| c == '\r' || c == '\u{2028}' || c == '\u{2029}' || is_stripped_control(c))
}

/// The text as the editor stores it: `\r\n`, lone `\r` and the Unicode line / paragraph
/// separators become `\n`; [`is_stripped_control`] characters are removed. Tabs, line feeds and
/// every printable character (bidirectional controls included) are kept.
#[must_use]
pub fn sanitize(text: &str) -> Cow<'_, str> {
    if !needs_sanitizing(text) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push('\n');
            }
            '\u{2028}' | '\u{2029}' => out.push('\n'),
            c if is_stripped_control(c) => {}
            c => out.push(c),
        }
    }
    Cow::Owned(out)
}

/// The minimal edits that turn `text` into [`sanitize`]`(text)`: byte range and replacement,
/// in order. Applied from the last to the first, they leave every other byte in place, so the
/// editor can repair a commit without rewriting the whole document.
#[must_use]
pub fn fixes(text: &str) -> Vec<(Range<usize>, &'static str)> {
    let mut out = Vec::new();
    let mut iter = text.char_indices().peekable();
    while let Some((i, c)) = iter.next() {
        match c {
            '\r' => {
                if iter.peek().map(|&(_, n)| n) == Some('\n') {
                    iter.next();
                    out.push((i..i + 2, "\n"));
                } else {
                    out.push((i..i + 1, "\n"));
                }
            }
            '\u{2028}' | '\u{2029}' => out.push((i..i + c.len_utf8(), "\n")),
            c if is_stripped_control(c) => out.push((i..i + c.len_utf8(), "")),
            _ => {}
        }
    }
    out
}

/// Applies [`fixes`] to a copy of `text` (for tests and non-editor callers).
#[must_use]
pub fn apply_fixes(text: &str, fixes: &[(Range<usize>, &str)]) -> String {
    let mut out = text.to_owned();
    for (range, rep) in fixes.iter().rev() {
        out.replace_range(range.clone(), rep);
    }
    out
}

/// One bidirectional control character found in a text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BidiControl {
    /// Byte range of the character.
    pub range: Range<usize>,
    /// The character.
    pub ch: char,
    /// Its Unicode name.
    pub name: &'static str,
}

impl BidiControl {
    /// `U+202E RIGHT-TO-LEFT OVERRIDE`.
    #[must_use]
    pub fn label(&self) -> String {
        format!("U+{:04X} {}", u32::from(self.ch), self.name)
    }

    /// A range to highlight for it: the character is zero-width once shaped, so the range also
    /// covers the next character when there is one on the same line (else the previous one).
    #[must_use]
    pub fn highlight_range(&self, text: &str) -> Range<usize> {
        let next = text[self.range.end..]
            .chars()
            .next()
            .filter(|c| *c != '\n')
            .map(char::len_utf8);
        if let Some(len) = next {
            return self.range.start..self.range.end + len;
        }
        let prev = text[..self.range.start]
            .chars()
            .next_back()
            .filter(|c| *c != '\n')
            .map(char::len_utf8);
        match prev {
            Some(len) => self.range.start - len..self.range.end,
            None => self.range.clone(),
        }
    }
}

/// Every bidirectional control character of `text`, in order.
#[must_use]
pub fn find_bidi_controls(text: &str) -> Vec<BidiControl> {
    text.char_indices()
        .filter_map(|(i, ch)| {
            bidi_control_name(ch).map(|name| BidiControl {
                range: i..i + ch.len_utf8(),
                ch,
                name,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn strips_controls_and_normalizes_line_ends() {
        assert_eq!(sanitize("select 1"), Cow::Borrowed("select 1"));
        assert_eq!(sanitize("a\u{0}b\u{1b}[31mc\u{7f}\u{85}d"), "ab[31mcd");
        assert_eq!(sanitize("a\r\nb\rc\u{2028}d"), "a\nb\nc\nd");
        assert_eq!(sanitize("\ttab\nline"), "\ttab\nline");
        assert!(!needs_sanitizing("\u{202e}rtl"));
    }

    #[test]
    fn finds_bidi_controls() {
        let text = "select '\u{202e}tpircs' -- \u{2066}x";
        let found = find_bidi_controls(text);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].label(), "U+202E RIGHT-TO-LEFT OVERRIDE");
        assert_eq!(&text[found[0].range.clone()], "\u{202e}");
        let hl = found[0].highlight_range(text);
        assert_eq!(&text[hl], "\u{202e}t");
        // At the end of the text: the previous character is highlighted with it.
        let end = "ab\u{200f}";
        let found = find_bidi_controls(end);
        assert_eq!(&end[found[0].highlight_range(end)], "b\u{200f}");
    }

    proptest! {
        #[test]
        fn sanitized_text_is_clean_and_stable(s in "\\PC*|[\\x00-\\x1f\\x7f-\\x9f\r\n a-z]*") {
            let once = sanitize(&s).into_owned();
            prop_assert!(!needs_sanitizing(&once));
            prop_assert_eq!(sanitize(&once).into_owned(), once.clone());
            prop_assert_eq!(find_bidi_controls(&once).len(), find_bidi_controls(&s).len());
            prop_assert_eq!(apply_fixes(&s, &fixes(&s)), once);
        }
    }
}
