//! Statement splitting through the dialect (F-SQL-03).
//!
//! The editor never splits or classifies SQL itself: [`StatementSplitter`] is the seam to the
//! dialect of the connection's driver. [`DialectSplitter`] adapts today's
//! [`Dialect::split_statements`]; once P3-D lands `Dialect::split_script` / `code_tokens`, the
//! adapter forwards to them (client directives and refused items included) and the interim
//! lexer below goes away.
//!
//! [`segments`](StatementSplitter::segments) only tells code from strings, quoted identifiers
//! and comments, for parameter detection and completion context. It is never used to decide
//! what runs or whether a statement is safe: classification stays in the drivers (ADR-0007).

use std::ops::Range;
use std::sync::Arc;

use ninesql_core::driver_api::{Dialect, LexicalRules};

/// What one item of a script is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ItemKind {
    /// An SQL statement, sent to the engine.
    Statement,
    /// A client-side directive (`@set`, `\connect`…), handled by the core, never sent as SQL.
    ClientDirective,
    /// An item the dialect refuses to run, with the reason to show.
    Refused(String),
}

/// One item of a script: its byte span (without surrounding whitespace, leading comments and the
/// terminating `;`) and its kind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScriptItem {
    /// Byte range inside the text that was split.
    pub span: Range<usize>,
    /// Statement, directive or refused.
    pub kind: ItemKind,
}

impl ScriptItem {
    /// The item text inside `text`.
    #[must_use]
    pub fn text<'a>(&self, text: &'a str) -> &'a str {
        text.get(self.span.clone()).unwrap_or("")
    }
}

/// What a lexical segment of SQL text is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SegmentKind {
    /// Keywords, names, operators, numbers, whitespace.
    Code,
    /// A string literal (including dollar-quoted bodies).
    String,
    /// A quoted identifier (`"…"`, `` `…` ``).
    QuotedIdentifier,
    /// A comment.
    Comment,
}

/// A contiguous run of one [`SegmentKind`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Segment {
    /// Byte range.
    pub range: Range<usize>,
    /// Kind.
    pub kind: SegmentKind,
}

/// The dialect's splitting, as the editor needs it. Implemented over the connection's
/// [`Dialect`] by [`DialectSplitter`]; tests may provide their own.
pub trait StatementSplitter {
    /// Splits `text` into items, in order, spans relative to `text`.
    fn split(&self, text: &str) -> Vec<ScriptItem>;

    /// Lexical segments of `text`, covering it entirely, in order.
    fn segments(&self, text: &str) -> Vec<Segment>;
}

/// [`StatementSplitter`] over a driver's [`Dialect`].
#[derive(Clone)]
pub struct DialectSplitter {
    dialect: Arc<dyn Dialect>,
}

impl DialectSplitter {
    /// Splits with `dialect` (from `Driver::dialect` / `Session::dialect` through the core).
    #[must_use]
    pub fn new(dialect: Arc<dyn Dialect>) -> Self {
        Self { dialect }
    }
}

impl std::fmt::Debug for DialectSplitter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DialectSplitter").finish_non_exhaustive()
    }
}

impl StatementSplitter for DialectSplitter {
    fn split(&self, text: &str) -> Vec<ScriptItem> {
        self.dialect
            .split_statements(text)
            .into_iter()
            .map(|s| ScriptItem {
                span: s.start..s.end,
                kind: ItemKind::Statement,
            })
            .collect()
    }

    fn segments(&self, text: &str) -> Vec<Segment> {
        lex_segments(text, self.dialect.lexical_rules())
    }
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80
}

/// End (exclusive) of a quoted run opened at `start` by `quote`; doubled quotes and, with
/// `backslash`, backslash escapes stay inside. Unterminated: the end of the text.
fn skip_quoted(bytes: &[u8], start: usize, quote: u8, backslash: bool) -> usize {
    let mut i = start + 1;
    while i < bytes.len() {
        let b = bytes[i];
        if backslash && b == b'\\' {
            i += 2;
            continue;
        }
        if b == quote {
            if bytes.get(i + 1) == Some(&quote) {
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    bytes.len()
}

fn dollar_tag(bytes: &[u8], start: usize) -> Option<usize> {
    let mut i = start + 1;
    if bytes.get(i).is_some_and(u8::is_ascii_digit) {
        return None;
    }
    while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
        i += 1;
    }
    (bytes.get(i) == Some(&b'$')).then_some(i + 1 - start)
}

fn find_sub(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    hay.get(from..)?
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

/// Interim lexer over [`LexicalRules`], until P3-D's `code_tokens`: only separates code from
/// strings, quoted identifiers and comments. Never used for classification.
#[must_use]
pub fn lex_segments(text: &str, rules: LexicalRules) -> Vec<Segment> {
    let bytes = text.as_bytes();
    let mut out: Vec<Segment> = Vec::new();
    let mut push = |range: Range<usize>, kind: SegmentKind| {
        if range.is_empty() {
            return;
        }
        if let Some(last) = out.last_mut()
            && last.kind == kind
            && last.range.end == range.start
        {
            last.range.end = range.end;
            return;
        }
        out.push(Segment { range, kind });
    };
    let mut code_start = 0;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        let prev_word = i > 0 && is_word_byte(bytes[i - 1]);
        let found: Option<(usize, SegmentKind)> = match b {
            b'\'' => {
                let e_prefix = rules.e_string_backslash
                    && i > 0
                    && matches!(bytes[i - 1], b'e' | b'E')
                    && !(i > 1 && is_word_byte(bytes[i - 2]));
                let backslash = rules.backslash_escapes || e_prefix;
                Some((skip_quoted(bytes, i, b'\'', backslash), SegmentKind::String))
            }
            b'"' if rules.backslash_escapes => {
                Some((skip_quoted(bytes, i, b'"', true), SegmentKind::String))
            }
            b'"' => Some((
                skip_quoted(bytes, i, b'"', false),
                SegmentKind::QuotedIdentifier,
            )),
            b'`' if rules.backtick_identifiers => Some((
                skip_quoted(bytes, i, b'`', false),
                SegmentKind::QuotedIdentifier,
            )),
            b'-' if bytes.get(i + 1) == Some(&b'-')
                && (!rules.dash_comment_needs_space
                    || bytes
                        .get(i + 2)
                        .is_none_or(|c| c.is_ascii_whitespace() || c.is_ascii_control())) =>
            {
                let end = find_sub(bytes, b"\n", i).unwrap_or(bytes.len());
                Some((end, SegmentKind::Comment))
            }
            b'#' if rules.hash_comments => {
                let end = find_sub(bytes, b"\n", i).unwrap_or(bytes.len());
                Some((end, SegmentKind::Comment))
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                let executable =
                    rules.executable_comments && matches!(bytes.get(i + 2), Some(b'!' | b'M'));
                if executable {
                    // MySQL / MariaDB executable comments are code; their delimiters are left
                    // in the code segment.
                    None
                } else {
                    let mut depth = 1usize;
                    let mut j = i + 2;
                    while j < bytes.len() && depth > 0 {
                        if rules.nested_block_comments
                            && bytes[j] == b'/'
                            && bytes.get(j + 1) == Some(&b'*')
                        {
                            depth += 1;
                            j += 2;
                        } else if bytes[j] == b'*' && bytes.get(j + 1) == Some(&b'/') {
                            depth -= 1;
                            j += 2;
                        } else {
                            j += 1;
                        }
                    }
                    Some((j.min(bytes.len()), SegmentKind::Comment))
                }
            }
            b'$' if rules.dollar_quotes && !prev_word => dollar_tag(bytes, i).map(|len| {
                let tag = &bytes[i..i + len];
                let end = find_sub(bytes, tag, i + len).map_or(bytes.len(), |p| p + len);
                (end, SegmentKind::String)
            }),
            _ => None,
        };
        match found {
            Some((end, kind)) => {
                push(code_start..i, SegmentKind::Code);
                push(i..end, kind);
                i = end;
                code_start = end;
            }
            None => i += 1,
        }
    }
    push(code_start..bytes.len(), SegmentKind::Code);
    // Byte scanning only stops on ASCII delimiters, so every boundary is a `char` boundary.
    out
}

/// The item containing `cursor`, or the one the cursor is right after or right before on the
/// same line (separated only by spaces, tabs and `;`). `None` on blank lines between items.
#[must_use]
pub fn item_at_cursor<'a>(
    text: &str,
    items: &'a [ScriptItem],
    cursor: usize,
) -> Option<&'a ScriptItem> {
    if let Some(item) = items
        .iter()
        .find(|it| it.span.start <= cursor && cursor <= it.span.end)
    {
        return Some(item);
    }
    let only_gap = |range: Range<usize>| {
        text.get(range)
            .is_some_and(|gap| gap.chars().all(|c| matches!(c, ' ' | '\t' | ';')))
    };
    if let Some(before) = items.iter().rev().find(|it| it.span.end <= cursor)
        && only_gap(before.span.end..cursor)
    {
        return Some(before);
    }
    items
        .iter()
        .find(|it| it.span.start >= cursor)
        .filter(|after| only_gap(cursor..after.span.start))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ninesql_core::driver_api::AnsiDialect;

    fn kinds(text: &str, rules: LexicalRules) -> Vec<(&str, SegmentKind)> {
        lex_segments(text, rules)
            .into_iter()
            .map(|s| (&text[s.range], s.kind))
            .collect()
    }

    #[test]
    fn segments_cover_the_text() {
        let text = "select 'a''b', \"x\"\"y\" -- c\n/* d */ $q$ e $q$ from t";
        let segs = lex_segments(text, LexicalRules::POSTGRES);
        let mut at = 0;
        for s in &segs {
            assert_eq!(s.range.start, at);
            at = s.range.end;
        }
        assert_eq!(at, text.len());
        let k = kinds(text, LexicalRules::POSTGRES);
        assert!(k.contains(&("'a''b'", SegmentKind::String)));
        assert!(k.contains(&("\"x\"\"y\"", SegmentKind::QuotedIdentifier)));
        assert!(k.contains(&("-- c", SegmentKind::Comment)));
        assert!(k.contains(&("/* d */", SegmentKind::Comment)));
        assert!(k.contains(&("$q$ e $q$", SegmentKind::String)));
    }

    #[test]
    fn dialect_rules_apply() {
        // MySQL: `#` comments, backslash escapes, `--` needs a space, double quotes are strings.
        let k = kinds("select 'a\\'b' # c\n, 1--1, \"s\"", LexicalRules::MYSQL);
        assert!(k.contains(&("'a\\'b'", SegmentKind::String)));
        assert!(k.contains(&("# c", SegmentKind::Comment)));
        assert!(k.contains(&("\"s\"", SegmentKind::String)));
        assert!(!k.iter().any(|(t, _)| t.starts_with("--1")));
        // PostgreSQL: nested comments, `E''` strings.
        let k = kinds("/* a /* b */ c */ E'x\\'y'", LexicalRules::POSTGRES);
        assert_eq!(k[0], ("/* a /* b */ c */", SegmentKind::Comment));
        assert!(k.contains(&("'x\\'y'", SegmentKind::String)));
        // `$1` is not a dollar quote.
        let k = kinds("select $1, $2", LexicalRules::POSTGRES);
        assert_eq!(k, vec![("select $1, $2", SegmentKind::Code)]);
    }

    #[test]
    fn dialect_splitter_uses_the_dialect() {
        let splitter = DialectSplitter::new(Arc::new(AnsiDialect));
        let text = "select 1; select ';' ;\n\nselect 3";
        let items = splitter.split(text);
        let texts: Vec<&str> = items.iter().map(|i| i.text(text)).collect();
        assert_eq!(texts, ["select 1", "select ';'", "select 3"]);
        assert!(items.iter().all(|i| i.kind == ItemKind::Statement));
    }

    #[test]
    fn statement_under_cursor() {
        let text = "select 1;  \n\nselect 2;\nselect 3";
        let splitter = DialectSplitter::new(Arc::new(AnsiDialect));
        let items = splitter.split(text);
        let at = |c: usize| item_at_cursor(text, &items, c).map(|i| i.text(text));
        assert_eq!(at(0), Some("select 1"));
        assert_eq!(at(3), Some("select 1"));
        assert_eq!(at(9), Some("select 1"), "right after the semicolon");
        assert_eq!(at(11), Some("select 1"), "trailing spaces on the same line");
        assert_eq!(at(12), None, "blank line");
        assert_eq!(at(13), Some("select 2"));
        assert_eq!(at(text.len()), Some("select 3"));
    }
}
