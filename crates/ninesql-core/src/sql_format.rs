//! SQL formatter (F-SQL-09): keyword case and clause layout on top of the dialect lexer
//! ([`ninesql_driver_api::code_tokens`]). Pure: no I/O, no session.
//!
//! # It never changes the meaning
//! - The text is split with the dialect's [`Dialect::split_script`]; everything outside
//!   statement items (client directives such as the MySQL `DELIMITER`, the text between
//!   statements, delimiters) is copied unchanged.
//! - Inside a statement only two things change:
//!   1. the **case of keywords**, and only for words that are reserved in every supported
//!      dialect (MySQL's reserved list, the strictest one: a reserved word is never an unquoted
//!      table name or alias, which MySQL compares case-sensitively on Linux), never a word right
//!      after a `.` (MySQL accepts a reserved word as a qualified name part);
//!   2. **whitespace between tokens**: a gap made only of ASCII whitespace may become a single space
//!      or a line break with indentation; an empty gap stays empty, except right after a comma
//!      where a line break is always safe. A gap holding a comment is copied unchanged, so is
//!      the gap between two string literals (PostgreSQL joins `'a'` and `'b'` only across a
//!      line break).
//! - Tokens themselves (strings, quoted identifiers, numbers, operators) are never touched, and
//!   comments are copied byte for byte. A statement holding a `;` (a compound statement body)
//!   or a MySQL executable comment keeps its whitespace: only the keyword case changes. A
//!   statement holding a `[` (a SQLite bracket-quoted name, which the shared lexer does not read
//!   as one token, or an array subscript) is left entirely as it is.
//!
//! The property tests check that formatting keeps the token sequence (keywords compared without
//! case) and every comment, over the classification corpus and random statements.

use ninesql_driver_api::{
    CodeToken, CodeTokenKind, Dialect, LexicalRules, ScriptItemKind, code_tokens,
};

/// How keywords are written.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum KeywordCase {
    /// `SELECT` (default).
    #[default]
    Upper,
    /// `select`.
    Lower,
    /// As typed.
    Preserve,
}

/// Formatter options.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct FormatOptions {
    /// Keyword case.
    pub keyword_case: KeywordCase,
    /// Spaces per indentation level (0 to 8, default 2).
    pub indent: usize,
    /// Put clauses (`FROM`, `WHERE`, `JOIN`…) and select-list items on their own lines; when
    /// `false`, only the keyword case changes.
    pub layout: bool,
}

impl Default for FormatOptions {
    fn default() -> Self {
        Self {
            keyword_case: KeywordCase::Upper,
            indent: 2,
            layout: true,
        }
    }
}

impl FormatOptions {
    /// Sets the keyword case.
    #[must_use]
    pub fn with_keyword_case(mut self, case: KeywordCase) -> Self {
        self.keyword_case = case;
        self
    }

    /// Sets the indentation.
    #[must_use]
    pub fn with_indent(mut self, indent: usize) -> Self {
        self.indent = indent;
        self
    }

    /// Sets the layout.
    #[must_use]
    pub fn with_layout(mut self, layout: bool) -> Self {
        self.layout = layout;
        self
    }
}

/// Keywords whose case may change: reserved in MySQL 8 (and keywords in PostgreSQL and
/// SQLite, which fold or ignore the case of unquoted words). Sorted for binary search.
const CASED_KEYWORDS: &[&str] = &[
    "ALL",
    "ALTER",
    "AND",
    "AS",
    "ASC",
    "BETWEEN",
    "BY",
    "CASE",
    "CHECK",
    "CONSTRAINT",
    "CREATE",
    "CROSS",
    "DEFAULT",
    "DELETE",
    "DESC",
    "DISTINCT",
    "DROP",
    "ELSE",
    "EXCEPT",
    "EXISTS",
    "FALSE",
    "FOREIGN",
    "FROM",
    "GROUP",
    "HAVING",
    "IN",
    "INDEX",
    "INNER",
    "INSERT",
    "INTERSECT",
    "INTO",
    "IS",
    "JOIN",
    "KEY",
    "LEFT",
    "LIKE",
    "LIMIT",
    "NATURAL",
    "NOT",
    "NULL",
    "ON",
    "OR",
    "ORDER",
    "OUTER",
    "PRIMARY",
    "REFERENCES",
    "RIGHT",
    "SELECT",
    "SET",
    "TABLE",
    "THEN",
    "TRUE",
    "UNION",
    "UNIQUE",
    "UPDATE",
    "USING",
    "VALUES",
    "WHEN",
    "WHERE",
    "WITH",
];

/// Words that start a clause on a new line.
const CLAUSES: &[&str] = &[
    "FROM",
    "WHERE",
    "GROUP",
    "ORDER",
    "HAVING",
    "LIMIT",
    "OFFSET",
    "UNION",
    "INTERSECT",
    "EXCEPT",
    "VALUES",
    "RETURNING",
    "WINDOW",
];

/// Words that start a join on a new line.
const JOIN_STARTS: &[&str] = &["JOIN", "LEFT", "RIGHT", "INNER", "FULL", "CROSS", "NATURAL"];

fn upper(word: &str) -> String {
    word.to_ascii_uppercase()
}

fn is_cased_keyword(word: &str) -> bool {
    CASED_KEYWORDS.binary_search(&upper(word).as_str()).is_ok()
}

/// Formats `text` with `dialect`'s lexical rules and splitter (see the [module
/// documentation](self)).
#[must_use]
pub fn format_sql(dialect: &dyn Dialect, text: &str, options: &FormatOptions) -> String {
    let rules = dialect.lexical_rules();
    let mut out = String::with_capacity(text.len() + text.len() / 8);
    let mut at = 0usize;
    for item in dialect.split_script(text) {
        if item.kind != ScriptItemKind::Statement {
            continue;
        }
        let (start, end) = (item.span.start, item.span.end);
        if start < at
            || end > text.len()
            || !text.is_char_boundary(start)
            || !text.is_char_boundary(end)
        {
            continue;
        }
        out.push_str(&text[at..start]);
        out.push_str(&format_statement(&text[start..end], rules, options));
        at = end;
    }
    out.push_str(&text[at..]);
    out
}

/// What the innermost open parenthesis holds.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Paren {
    /// A subquery: clauses inside are laid out.
    Query,
    /// Anything else (a call, a list): kept on its line.
    Other,
}

fn format_statement(text: &str, rules: LexicalRules, options: &FormatOptions) -> String {
    let tokens = code_tokens(text, rules);
    // `[` starts a quoted name in SQLite (`[a  "b]`), which the shared lexer does not read as
    // one token: such a statement (and PostgreSQL array subscripts with it) is left as it is.
    if tokens.is_empty() || tokens.iter().any(|t| t.text(text) == "[") {
        return text.to_owned();
    }
    let compound = tokens.iter().any(|t| t.kind == CodeTokenKind::Semicolon);
    let executable = rules.executable_comments && (text.contains("/*!") || text.contains("/*M!"));
    let layout = options.layout && !compound && !executable;
    let indent = options.indent.min(8);
    let first_word = tokens
        .first()
        .filter(|t| t.kind == CodeTokenKind::Word)
        .map(|t| upper(t.text(text)));
    let mut out = String::with_capacity(text.len() + 32);
    out.push_str(&text[..tokens[0].start]);
    let mut parens: Vec<Paren> = Vec::new();
    // Clause of each level: `SELECT` lists get one item per line.
    let mut in_select: Vec<bool> = vec![false];
    for (i, tok) in tokens.iter().enumerate() {
        if i > 0 {
            let prev = &tokens[i - 1];
            let gap = &text[prev.end..tok.start];
            let laid_out = layout && gap_is_free(gap, prev, tok);
            let query_level = parens.last().is_none_or(|p| *p == Paren::Query);
            let depth = parens.iter().filter(|p| **p == Paren::Query).count();
            let base = depth * indent;
            let breaks =
                laid_out && query_level && breaks_before(&tokens, i, text, first_word.as_deref());
            let after_comma = layout
                && query_level
                && prev.text(text) == ","
                && *in_select.last().unwrap_or(&false)
                && gap.bytes().all(|b| b.is_ascii_whitespace());
            if breaks {
                out.push('\n');
                push_spaces(&mut out, base);
            } else if after_comma {
                out.push('\n');
                push_spaces(&mut out, base + indent);
            } else if laid_out && !gap.is_empty() {
                out.push(' ');
            } else {
                out.push_str(gap);
            }
        }
        let word = tok.text(text);
        match tok.kind {
            CodeTokenKind::Word => {
                let w = upper(word);
                if w == "SELECT" {
                    if let Some(s) = in_select.last_mut() {
                        *s = true;
                    }
                } else if CLAUSES.contains(&w.as_str())
                    && let Some(s) = in_select.last_mut()
                {
                    *s = false;
                }
                let after_dot = i > 0 && tokens[i - 1].text(text) == ".";
                if !after_dot && is_cased_keyword(word) {
                    match options.keyword_case {
                        KeywordCase::Upper => out.push_str(&word.to_ascii_uppercase()),
                        KeywordCase::Lower => out.push_str(&word.to_ascii_lowercase()),
                        KeywordCase::Preserve => out.push_str(word),
                    }
                } else {
                    out.push_str(word);
                }
            }
            CodeTokenKind::OpenParen => {
                let next_is_query = tokens.get(i + 1).is_some_and(|n| {
                    n.is_word(text, "SELECT")
                        || n.is_word(text, "WITH")
                        || n.is_word(text, "VALUES")
                });
                parens.push(if next_is_query {
                    Paren::Query
                } else {
                    Paren::Other
                });
                in_select.push(false);
                out.push_str(word);
            }
            CodeTokenKind::CloseParen => {
                parens.pop();
                if in_select.len() > 1 {
                    in_select.pop();
                }
                out.push_str(word);
            }
            _ => out.push_str(word),
        }
    }
    if let Some(last) = tokens.last() {
        out.push_str(&text[last.end..]);
    }
    out
}

fn push_spaces(out: &mut String, n: usize) {
    for _ in 0..n {
        out.push(' ');
    }
}

/// A gap that may be rewritten: non-empty, ASCII whitespace only (an engine may read another
/// space character as part of a name), not between two string literals.
fn gap_is_free(gap: &str, prev: &CodeToken, next: &CodeToken) -> bool {
    !gap.is_empty()
        && gap.bytes().all(|b| b.is_ascii_whitespace())
        && !(prev.kind == CodeTokenKind::Str && next.kind == CodeTokenKind::Str)
}

/// `true` when token `i` starts a clause or a join.
fn breaks_before(tokens: &[CodeToken], i: usize, text: &str, first_word: Option<&str>) -> bool {
    let tok = &tokens[i];
    if tok.kind != CodeTokenKind::Word {
        return false;
    }
    let prev = &tokens[i - 1];
    if prev.text(text) == "." {
        return false;
    }
    let next_is = |w: &str| tokens.get(i + 1).is_some_and(|n| n.is_word(text, w));
    let next_is_paren = tokens
        .get(i + 1)
        .is_some_and(|n| n.kind == CodeTokenKind::OpenParen);
    let prev_word = upper(prev.text(text));
    let w = upper(tok.text(text));
    match w.as_str() {
        "SELECT" => prev.kind != CodeTokenKind::OpenParen,
        "GROUP" | "ORDER" => next_is("BY"),
        "SET" => first_word == Some("UPDATE"),
        "VALUES" => first_word == Some("INSERT") || first_word == Some("REPLACE"),
        "JOIN" => !JOIN_STARTS.contains(&prev_word.as_str()) && prev_word != "OUTER",
        "LEFT" | "RIGHT" => !next_is_paren && (next_is("JOIN") || next_is("OUTER")),
        "INNER" | "FULL" | "CROSS" | "NATURAL" => {
            !JOIN_STARTS.contains(&prev_word.as_str()) && !next_is_paren
        }
        w => CLAUSES.contains(&w) && !next_is_paren,
    }
}

#[cfg(test)]
mod tests {
    use ninesql_driver_api::AnsiDialect;

    use super::*;

    #[test]
    fn keyword_list_is_sorted() {
        let mut sorted = CASED_KEYWORDS.to_vec();
        sorted.sort_unstable();
        assert_eq!(sorted, CASED_KEYWORDS);
    }

    #[test]
    fn lays_out_a_query() {
        let sql = "select a, b.select, count(*) from t left join u on t.id = u.id where a = 'x  y' and b in (select c from v) group by a order by 2";
        let out = format_sql(&AnsiDialect, sql, &FormatOptions::default());
        assert_eq!(
            out,
            "SELECT a,\n  b.select,\n  count(*)\nFROM t\nLEFT JOIN u ON t.id = u.id\nWHERE a = 'x  y' AND b IN (SELECT c\n  FROM v)\nGROUP BY a\nORDER BY 2"
        );
    }

    #[test]
    fn comments_strings_and_directives_are_untouched() {
        let sql =
            "select 1 -- keep  this\n  from t /* and  this */ where x = 'a'\n'b';\n\nselect   2";
        let out = format_sql(
            &AnsiDialect,
            sql,
            &FormatOptions::default().with_keyword_case(KeywordCase::Lower),
        );
        assert!(out.contains("-- keep  this\n  from"));
        assert!(out.contains("/* and  this */"));
        assert!(out.contains("'a'\n'b'"));
        assert!(out.ends_with(";\n\nselect 2"));
    }

    #[test]
    fn no_layout_keeps_whitespace() {
        let sql = "select  a ,b\nfrom   t";
        let out = format_sql(
            &AnsiDialect,
            sql,
            &FormatOptions::default().with_layout(false),
        );
        assert_eq!(out, "SELECT  a ,b\nFROM   t");
    }
}
