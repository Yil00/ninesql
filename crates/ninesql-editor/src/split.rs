//! Statement splitting through the dialect (F-SQL-03).
//!
//! The editor never splits or classifies SQL itself: [`StatementSplitter`] is the seam to the
//! dialect of the connection's driver. [`DialectSplitter`] forwards to
//! [`Dialect::split_script`] (statements, client directives such as MySQL `DELIMITER`, and
//! refused pieces such as `COPY … FROM STDIN` data or psql meta-commands) and builds its
//! segments from the driver API's shared lexer ([`code_tokens`]).
//!
//! [`segments`](StatementSplitter::segments) only tells code from strings, quoted identifiers
//! and comments, for parameter detection and completion context. It is never used to decide
//! what runs or whether a statement is safe: classification stays in the drivers (ADR-0007).

use std::ops::Range;
use std::sync::Arc;

use ninesql_core::driver_api::{CodeTokenKind, Dialect, LexicalRules, ScriptItemKind, code_tokens};

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
            .split_script(text)
            .into_iter()
            .map(|item| ScriptItem {
                span: item.span.start..item.span.end,
                kind: match item.kind {
                    ScriptItemKind::Statement => ItemKind::Statement,
                    ScriptItemKind::ClientDirective => ItemKind::ClientDirective,
                    ScriptItemKind::Refused(why) => ItemKind::Refused(why.message().to_owned()),
                    // Fail-closed: a kind this editor does not know is never sent as SQL.
                    _ => ItemKind::Refused("this part of the script cannot run".to_owned()),
                },
            })
            .collect()
    }

    fn segments(&self, text: &str) -> Vec<Segment> {
        code_segments(text, self.dialect.lexical_rules())
    }
}

/// Segments of `text` from the shared lexer of the driver API ([`code_tokens`], the tokens the
/// splitter and the classifier read): string literals and quoted identifiers are the lexer's
/// tokens, everything else between tokens is code, except comments, which only live between
/// tokens (blanks and comments are what [`code_tokens`] skips). A string's prefix (`E`, `U&`,
/// `N`…) is code, so a segment of kind [`SegmentKind::String`] starts at its quote.
#[must_use]
pub fn code_segments(text: &str, rules: LexicalRules) -> Vec<Segment> {
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
    let mut at = 0;
    for tok in code_tokens(text, rules) {
        let (start, end) = (tok.start.max(at), tok.end.min(text.len()));
        if start > at {
            gap_segments(text, at..start, rules, &mut push);
        }
        if end <= start {
            continue;
        }
        match tok.kind {
            CodeTokenKind::Str => {
                let quote = text[start..end]
                    .find(['\'', '$', '"'])
                    .map_or(start, |p| start + p);
                push(start..quote, SegmentKind::Code);
                push(quote..end, SegmentKind::String);
            }
            // MySQL / MariaDB without `ANSI_QUOTES`: `"…"` is a string (the lexer reads it as
            // a quoted token either way; only its role differs).
            CodeTokenKind::QuotedIdent
                if rules.double_quote_backslash && text[start..end].starts_with('"') =>
            {
                push(start..end, SegmentKind::String);
            }
            CodeTokenKind::QuotedIdent => push(start..end, SegmentKind::QuotedIdentifier),
            _ => push(start..end, SegmentKind::Code),
        }
        at = end;
    }
    if at < text.len() {
        gap_segments(text, at..text.len(), rules, &mut push);
    }
    out
}

/// Splits a gap between two tokens (only blanks and comments, by construction of
/// [`code_tokens`]) into code (blanks, executable-comment markers) and comments. A block comment
/// still open at the end of the gap runs to its end: the lexer found no token before it.
fn gap_segments(
    text: &str,
    gap: Range<usize>,
    rules: LexicalRules,
    push: &mut impl FnMut(Range<usize>, SegmentKind),
) {
    let bytes = text.as_bytes();
    let mut i = gap.start;
    let mut code_start = gap.start;
    while i < gap.end {
        let rest = &bytes[i..gap.end];
        let comment_end = if rest.starts_with(b"/*") && !rest.starts_with(b"/*!") {
            let mut depth = 0usize;
            let mut j = i;
            let mut end = gap.end;
            while j + 1 < gap.end {
                if bytes[j] == b'/' && bytes[j + 1] == b'*' {
                    depth += 1;
                    j += 2;
                } else if bytes[j] == b'*' && bytes[j + 1] == b'/' {
                    depth = depth.saturating_sub(1);
                    j += 2;
                    if depth == 0 {
                        end = j;
                        break;
                    }
                } else {
                    j += 1;
                }
            }
            Some(end)
        } else if rest.starts_with(b"--") || rest.starts_with(b"#") {
            let stop = |b: &u8| *b == b'\n' || (rules.line_comment_ends_at_cr && *b == b'\r');
            Some(rest.iter().position(stop).map_or(gap.end, |p| i + p))
        } else {
            None
        };
        match comment_end {
            Some(end) => {
                push(code_start..i, SegmentKind::Code);
                push(i..end, SegmentKind::Comment);
                i = end;
                code_start = end;
            }
            None => i += 1,
        }
    }
    // Gaps are cut at token boundaries and comment delimiters are ASCII: every boundary is a
    // `char` boundary.
    push(code_start..gap.end, SegmentKind::Code);
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
        code_segments(text, rules)
            .into_iter()
            .map(|s| (&text[s.range], s.kind))
            .collect()
    }

    #[test]
    fn segments_cover_the_text() {
        let text = "select 'a''b', \"x\"\"y\" -- c\n/* d */ $q$ e $q$ from t";
        let segs = code_segments(text, LexicalRules::POSTGRES);
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

    /// A dialect whose script splitter reports a client directive and a refused piece.
    struct Scripted;
    impl Dialect for Scripted {
        fn split_script(&self, script: &str) -> Vec<ninesql_core::driver_api::ScriptItem> {
            use ninesql_core::driver_api::{ScriptRefusal, StatementSpan, script_items};
            let spans = self.split_statements(script);
            script_items(
                script,
                spans.into_iter().map(|s| {
                    let kind = match &script[s.start..s.end] {
                        t if t.starts_with("DELIMITER") => ScriptItemKind::ClientDirective,
                        t if t.starts_with('\\') => {
                            ScriptItemKind::Refused(ScriptRefusal::PsqlMetaCommand)
                        }
                        _ => ScriptItemKind::Statement,
                    };
                    (
                        StatementSpan {
                            start: s.start,
                            end: s.end,
                        },
                        kind,
                    )
                }),
            )
        }
    }

    #[test]
    fn script_items_keep_their_kind() {
        let splitter = DialectSplitter::new(Arc::new(Scripted));
        let text = "select 1; DELIMITER //; \\connect x; select 2";
        let kinds: Vec<ItemKind> = splitter.split(text).into_iter().map(|i| i.kind).collect();
        assert_eq!(
            kinds,
            [
                ItemKind::Statement,
                ItemKind::ClientDirective,
                ItemKind::Refused("psql meta-commands are not supported".to_owned()),
                ItemKind::Statement,
            ]
        );
    }

    #[test]
    fn segments_follow_the_shared_lexer() {
        // An unterminated block comment runs to the end; a line comment stops before the
        // newline; executable comment content is code.
        let k = kinds("select 1 /* open", LexicalRules::POSTGRES);
        assert_eq!(k.last(), Some(&("/* open", SegmentKind::Comment)));
        let k = kinds("select 1 -- c\nfrom t", LexicalRules::ANSI);
        assert!(k.contains(&("-- c", SegmentKind::Comment)));
        assert!(
            k.iter()
                .any(|(t, kind)| t.contains("from t") && *kind == SegmentKind::Code)
        );
        let k = kinds("select /*!50000 1 */ from t", LexicalRules::MYSQL);
        assert!(
            k.iter()
                .any(|(t, kind)| t.contains('1') && *kind == SegmentKind::Code)
        );
        // Unicode text keeps char boundaries.
        let text = "select 'é' /* ü */ \"ñ\"";
        let segs = code_segments(text, LexicalRules::POSTGRES);
        assert!(segs.iter().all(|s| text.is_char_boundary(s.range.start)
            && text.is_char_boundary(s.range.end)));
    }

    proptest::proptest! {
        #[test]
        fn segments_always_cover_the_text(text in "[ a-z'\"`$#;/*\\\n-é]{0,64}") {
            for rules in [LexicalRules::ANSI, LexicalRules::POSTGRES, LexicalRules::MYSQL, LexicalRules::SQLITE] {
                let segs = code_segments(&text, rules);
                let mut at = 0;
                for s in &segs {
                    proptest::prop_assert_eq!(s.range.start, at);
                    proptest::prop_assert!(s.range.end > s.range.start);
                    proptest::prop_assert!(text.is_char_boundary(s.range.end));
                    at = s.range.end;
                }
                proptest::prop_assert_eq!(at, text.len());
            }
        }
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
