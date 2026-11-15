//! What the cursor expects (tables, columns, keywords) and which tables are in scope with their
//! aliases, from a light token walk over the statement under the cursor. Only completion uses it.

use std::ops::Range;

use crate::split::{SegmentKind, StatementSplitter};

use super::keywords::KeywordSet;

#[derive(Clone, Debug, PartialEq, Eq)]
enum TokKind {
    /// Unquoted word (keyword or identifier).
    Word,
    /// Quoted identifier, unquoted value in `value`.
    Quoted,
    /// One punctuation character.
    Punct(char),
}

#[derive(Clone, Debug)]
struct Tok {
    range: Range<usize>,
    kind: TokKind,
    value: String,
}

impl Tok {
    fn is_word(&self, upper: &str) -> bool {
        self.kind == TokKind::Word && self.value.eq_ignore_ascii_case(upper)
    }

    fn is_punct(&self, c: char) -> bool {
        self.kind == TokKind::Punct(c)
    }
}

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

fn unquote(raw: &str) -> String {
    let mut chars = raw.chars();
    let Some(q) = chars.next() else {
        return String::new();
    };
    let inner = raw[q.len_utf8()..]
        .strip_suffix(q)
        .unwrap_or(&raw[q.len_utf8()..]);
    let doubled: String = [q, q].iter().collect();
    inner.replace(&doubled, &q.to_string())
}

fn tokenize(text: &str, splitter: &dyn StatementSplitter) -> Vec<Tok> {
    let mut out = Vec::new();
    for seg in splitter.segments(text) {
        match seg.kind {
            SegmentKind::Comment | SegmentKind::String => {}
            SegmentKind::QuotedIdentifier => out.push(Tok {
                range: seg.range.clone(),
                kind: TokKind::Quoted,
                value: unquote(&text[seg.range]),
            }),
            SegmentKind::Code => {
                let code = &text[seg.range.clone()];
                let mut iter = code.char_indices().peekable();
                while let Some((i, c)) = iter.next() {
                    let start = seg.range.start + i;
                    if c.is_whitespace() {
                        continue;
                    }
                    if is_ident_char(c) {
                        let mut end = start + c.len_utf8();
                        while let Some(&(j, d)) = iter.peek() {
                            if !is_ident_char(d) {
                                break;
                            }
                            end = seg.range.start + j + d.len_utf8();
                            iter.next();
                        }
                        out.push(Tok {
                            range: start..end,
                            kind: TokKind::Word,
                            value: text[start..end].to_owned(),
                        });
                    } else {
                        out.push(Tok {
                            range: start..start + c.len_utf8(),
                            kind: TokKind::Punct(c),
                            value: c.to_string(),
                        });
                    }
                }
            }
        }
    }
    out
}

/// A table (or CTE, or derived table) referenced by the statement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableRef {
    /// Qualified name parts, unquoted (`["public", "users"]`); empty for a derived table.
    pub parts: Vec<String>,
    /// Its alias, if any.
    pub alias: Option<String>,
}

impl TableRef {
    /// The table name (last part), if not derived.
    #[must_use]
    pub fn table(&self) -> Option<&str> {
        self.parts.last().map(String::as_str)
    }

    /// The schema part, if qualified.
    #[must_use]
    pub fn schema(&self) -> Option<&str> {
        (self.parts.len() >= 2).then(|| self.parts[self.parts.len() - 2].as_str())
    }

    /// Whether `name` designates this reference: its alias, or its table name when it has no
    /// alias (ASCII case-insensitive).
    #[must_use]
    pub fn answers_to(&self, name: &str) -> bool {
        match &self.alias {
            Some(alias) => alias.eq_ignore_ascii_case(name),
            None => self.table().is_some_and(|t| t.eq_ignore_ascii_case(name)),
        }
    }
}

/// What the cursor position expects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Expect {
    /// Inside a string or a comment: nothing.
    Nothing,
    /// Statement start or after a table reference: keywords.
    Keywords,
    /// After `FROM`, `JOIN`, `UPDATE`, `INTO`, `TABLE` or a comma in a `FROM` list.
    Tables,
    /// In an expression: columns of the tables in scope, aliases and keywords.
    Columns,
    /// After `qualifier.`: the columns of that alias / table, or the objects of that schema.
    Qualified,
}

/// The analysis of a cursor position.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CursorContext {
    /// The identifier being typed (may be empty).
    pub prefix: String,
    /// Its byte range in the document (what a completion replaces).
    pub prefix_range: Range<usize>,
    /// Identifiers before `.` (`a.b.` gives `["a", "b"]`), unquoted.
    pub qualifier: Vec<String>,
    /// What the position expects.
    pub expect: Expect,
    /// Tables referenced anywhere in the statement.
    pub scope: Vec<TableRef>,
    /// CTE names defined by the statement.
    pub ctes: Vec<String>,
}

const TABLE_KEYWORDS: &[&str] = &["FROM", "JOIN", "UPDATE", "INTO", "TABLE", "TRUNCATE"];
const EXPR_CLAUSES: &[&str] = &[
    "SELECT",
    "WHERE",
    "ON",
    "BY",
    "HAVING",
    "SET",
    "AND",
    "OR",
    "NOT",
    "WHEN",
    "THEN",
    "ELSE",
    "RETURNING",
    "VALUES",
    "DISTINCT",
    "CASE",
    "USING",
];
const CLAUSES: &[&str] = &[
    "SELECT",
    "FROM",
    "JOIN",
    "WHERE",
    "ON",
    "BY",
    "HAVING",
    "SET",
    "INTO",
    "UPDATE",
    "VALUES",
    "RETURNING",
    "LIMIT",
    "WITH",
    "USING",
];

/// The byte range of the statement around `cursor`: the dialect's item containing it, extended
/// to the cursor while no `;` separates them.
fn statement_region(text: &str, cursor: usize, splitter: &dyn StatementSplitter) -> Range<usize> {
    let items = splitter.split(text);
    if let Some(item) = items
        .iter()
        .find(|it| it.span.start <= cursor && cursor <= it.span.end)
    {
        return item.span.clone();
    }
    let segs = splitter.segments(text);
    let code_semicolon_in = |range: Range<usize>| {
        segs.iter()
            .filter(|s| s.kind == SegmentKind::Code)
            .filter_map(|s| {
                let lo = s.range.start.max(range.start);
                let hi = s.range.end.min(range.end);
                (lo < hi).then(|| text[lo..hi].rfind(';').map(|p| lo + p))
            })
            .flatten()
            .max()
    };
    let start = match items.iter().rev().find(|it| it.span.end <= cursor) {
        Some(prev) => match code_semicolon_in(prev.span.end..cursor) {
            Some(semi) => semi + 1,
            None => prev.span.start,
        },
        None => 0,
    };
    let end = items
        .iter()
        .find(|it| it.span.start > cursor)
        .map_or(text.len(), |next| next.span.start);
    start..end.max(cursor)
}

fn in_non_code(text: &str, cursor: usize, splitter: &dyn StatementSplitter) -> bool {
    splitter.segments(text).iter().any(|s| match s.kind {
        SegmentKind::Code => false,
        SegmentKind::Comment => {
            let seg = &text[s.range.clone()];
            let line_comment = seg.starts_with("--") || seg.starts_with('#');
            s.range.start < cursor
                && (cursor < s.range.end || (line_comment && cursor == s.range.end))
        }
        SegmentKind::String | SegmentKind::QuotedIdentifier => {
            let seg = &text[s.range.clone()];
            let closed = seg.chars().count() >= 2
                && seg.chars().next() == seg.chars().last()
                && !seg.starts_with('$');
            s.range.start < cursor
                && (cursor < s.range.end
                    || (cursor == s.range.end && !closed && !seg.starts_with('$')))
        }
    })
}

fn is_name(tok: &Tok, keywords: KeywordSet) -> bool {
    match tok.kind {
        TokKind::Quoted => true,
        TokKind::Word => {
            !keywords.contains(&tok.value)
                && !tok.value.chars().next().is_some_and(|c| c.is_ascii_digit())
        }
        TokKind::Punct(_) => false,
    }
}

/// Skips a parenthesised group starting at `i` (which must be `(`); returns the index after `)`.
fn skip_group(toks: &[Tok], i: usize) -> usize {
    let mut depth = 0usize;
    let mut j = i;
    while j < toks.len() {
        if toks[j].is_punct('(') {
            depth += 1;
        } else if toks[j].is_punct(')') {
            depth = depth.saturating_sub(1);
            if depth == 0 {
                return j + 1;
            }
        }
        j += 1;
    }
    toks.len()
}

fn parse_scope(toks: &[Tok], keywords: KeywordSet) -> (Vec<TableRef>, Vec<String>) {
    let mut refs = Vec::new();
    let mut ctes = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        let t = &toks[i];
        if t.is_word("WITH") {
            let mut j = i + 1;
            if toks.get(j).is_some_and(|t| t.is_word("RECURSIVE")) {
                j += 1;
            }
            // Collect every CTE name; the bodies are scanned by the outer loop (they may
            // reference tables too), so `i` only moves past `WITH`.
            while let Some(name) = toks.get(j).filter(|t| is_name(t, keywords)) {
                ctes.push(name.value.clone());
                j += 1;
                if toks.get(j).is_some_and(|t| t.is_punct('(')) {
                    j = skip_group(toks, j);
                }
                if toks.get(j).is_some_and(|t| t.is_word("AS")) {
                    j += 1;
                }
                if toks.get(j).is_some_and(|t| t.is_word("NOT")) {
                    j += 1;
                }
                if toks.get(j).is_some_and(|t| t.is_word("MATERIALIZED")) {
                    j += 1;
                }
                if !toks.get(j).is_some_and(|t| t.is_punct('(')) {
                    break;
                }
                j = skip_group(toks, j);
                if !toks.get(j).is_some_and(|t| t.is_punct(',')) {
                    break;
                }
                j += 1;
            }
            i += 1;
            continue;
        }
        let triggers = TABLE_KEYWORDS.iter().any(|k| t.is_word(k));
        if !triggers {
            i += 1;
            continue;
        }
        let list = t.is_word("FROM");
        let mut j = i + 1;
        loop {
            let mut parts = Vec::new();
            if toks.get(j).is_some_and(|t| t.is_punct('(')) {
                let after = skip_group(toks, j);
                // A derived table: its body is scanned on its own by the outer loop.
                if toks.get(j + 1).is_some_and(|t| t.is_word("SELECT")) {
                    let mut k = after;
                    if toks.get(k).is_some_and(|t| t.is_word("AS")) {
                        k += 1;
                    }
                    if let Some(alias) = toks.get(k).filter(|t| is_name(t, keywords)) {
                        refs.push(TableRef {
                            parts: Vec::new(),
                            alias: Some(alias.value.clone()),
                        });
                    }
                }
                break;
            }
            while let Some(name) = toks.get(j).filter(|t| is_name(t, keywords)) {
                parts.push(name.value.clone());
                j += 1;
                if toks.get(j).is_some_and(|t| t.is_punct('.'))
                    && toks.get(j + 1).is_some_and(|t| is_name(t, keywords))
                {
                    j += 1;
                } else {
                    break;
                }
            }
            if parts.is_empty() {
                break;
            }
            if toks.get(j).is_some_and(|t| t.is_word("AS")) {
                j += 1;
            }
            let alias = toks
                .get(j)
                .filter(|t| is_name(t, keywords))
                .map(|t| t.value.clone());
            if alias.is_some() {
                j += 1;
            }
            refs.push(TableRef { parts, alias });
            if list && toks.get(j).is_some_and(|t| t.is_punct(',')) {
                j += 1;
                continue;
            }
            break;
        }
        i = j.max(i + 1);
    }
    (refs, ctes)
}

/// Analyses the cursor position `cursor` (byte offset) of `text`.
#[must_use]
pub fn analyze(
    text: &str,
    cursor: usize,
    splitter: &dyn StatementSplitter,
    keywords: KeywordSet,
) -> CursorContext {
    let cursor = crate::text::floor_char_boundary(text, cursor);
    let prefix_start = text[..cursor]
        .char_indices()
        .rev()
        .take_while(|&(_, c)| is_ident_char(c))
        .last()
        .map_or(cursor, |(i, _)| i);
    let prefix = text[prefix_start..cursor].to_owned();
    let mut ctx = CursorContext {
        prefix,
        prefix_range: prefix_start..cursor,
        qualifier: Vec::new(),
        expect: Expect::Nothing,
        scope: Vec::new(),
        ctes: Vec::new(),
    };
    if in_non_code(text, cursor, splitter) {
        return ctx;
    }
    let region = statement_region(text, cursor, splitter);
    let region_text = &text[region.clone()];
    let mut toks = tokenize(region_text, splitter);
    for t in &mut toks {
        t.range = t.range.start + region.start..t.range.end + region.start;
    }
    let (scope, ctes) = parse_scope(&toks, keywords);
    ctx.scope = scope;
    ctx.ctes = ctes;

    let before: Vec<&Tok> = toks
        .iter()
        .filter(|t| t.range.end <= prefix_start)
        .collect();
    // Qualifier chain, written without spaces: `name.name.<prefix>`.
    let mut k = before.len();
    let mut expected_end = prefix_start;
    while k >= 2
        && before[k - 1].is_punct('.')
        && before[k - 1].range.end == expected_end
        && matches!(before[k - 2].kind, TokKind::Word | TokKind::Quoted)
        && before[k - 2].range.end == before[k - 1].range.start
    {
        ctx.qualifier.insert(0, before[k - 2].value.clone());
        expected_end = before[k - 2].range.start;
        k -= 2;
    }
    if !ctx.qualifier.is_empty() {
        ctx.expect = Expect::Qualified;
        return ctx;
    }
    let rest = &before[..k];
    let Some(last) = rest.last() else {
        ctx.expect = Expect::Keywords;
        return ctx;
    };
    // The innermost clause keyword at the cursor's parenthesis depth.
    let mut depth = 0i32;
    let mut clause: Vec<(i32, String)> = Vec::new();
    for t in rest {
        if t.is_punct('(') {
            depth += 1;
        } else if t.is_punct(')') {
            clause.retain(|(d, _)| *d < depth);
            depth -= 1;
        } else if t.kind == TokKind::Word {
            let upper = t.value.to_ascii_uppercase();
            if CLAUSES.contains(&upper.as_str()) {
                clause.retain(|(d, _)| *d != depth);
                clause.push((depth, upper));
            }
        }
    }
    let current = clause.last().map(|(_, c)| c.as_str());
    let last_upper = last.value.to_ascii_uppercase();
    let after_table_keyword =
        last.kind == TokKind::Word && TABLE_KEYWORDS.contains(&last_upper.as_str());
    ctx.expect = if after_table_keyword || (last.is_punct(',') && current == Some("FROM")) {
        Expect::Tables
    } else if (is_name(last, keywords) || last.is_punct(')'))
        && matches!(current, Some("FROM" | "JOIN" | "UPDATE" | "INTO"))
    {
        Expect::Keywords
    } else if last.kind == TokKind::Word && last_upper == "AS" {
        Expect::Nothing
    } else if current.is_some_and(|c| EXPR_CLAUSES.contains(&c))
        || (last.kind == TokKind::Word && EXPR_CLAUSES.contains(&last_upper.as_str()))
        || matches!(
            last.kind,
            TokKind::Punct('=' | '<' | '>' | '(' | ',' | '+' | '-' | '*' | '/')
        )
    {
        Expect::Columns
    } else {
        Expect::Keywords
    };
    ctx
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::split::DialectSplitter;
    use ninesql_core::driver_api::AnsiDialect;
    use std::sync::Arc;

    fn at(text_with_cursor: &str) -> CursorContext {
        let cursor = text_with_cursor.find('|').unwrap_or(text_with_cursor.len());
        let text = text_with_cursor.replacen('|', "", 1);
        let splitter = DialectSplitter::new(Arc::new(AnsiDialect));
        analyze(&text, cursor, &splitter, KeywordSet::Ansi)
    }

    #[test]
    fn expectations() {
        assert_eq!(at("sel|").expect, Expect::Keywords);
        assert_eq!(at("select * from |").expect, Expect::Tables);
        assert_eq!(at("select * from us|").expect, Expect::Tables);
        assert_eq!(at("select * from users u, |").expect, Expect::Tables);
        assert_eq!(at("select * from users u |").expect, Expect::Keywords);
        assert_eq!(at("select | from users").expect, Expect::Columns);
        assert_eq!(at("select * from users where |").expect, Expect::Columns);
        assert_eq!(
            at("select * from a join b on a.id = |").expect,
            Expect::Columns
        );
        assert_eq!(
            at("select * from t where x = 'abc|'").expect,
            Expect::Nothing
        );
        assert_eq!(at("select 1 -- from |").expect, Expect::Nothing);
        assert_eq!(at("insert into |").expect, Expect::Tables);
        assert_eq!(at("update t set |").expect, Expect::Columns);
    }

    #[test]
    fn qualifier_and_prefix() {
        let c = at("select u.na| from users u");
        assert_eq!(c.expect, Expect::Qualified);
        assert_eq!(c.qualifier, ["u"]);
        assert_eq!(c.prefix, "na");
        let c = at("select * from public.|");
        assert_eq!(c.qualifier, ["public"]);
        let c = at("select \"My T\".| from \"My T\"");
        assert_eq!(c.qualifier, ["My T"]);
        assert_eq!(c.prefix, "");
    }

    #[test]
    fn scope_resolves_aliases() {
        let c = at("select | from public.users as u, items join orders o on o.user_id = u.id");
        assert_eq!(
            c.scope,
            vec![
                TableRef {
                    parts: vec!["public".into(), "users".into()],
                    alias: Some("u".into())
                },
                TableRef {
                    parts: vec!["items".into()],
                    alias: None
                },
                TableRef {
                    parts: vec!["orders".into()],
                    alias: Some("o".into())
                },
            ]
        );
        assert!(c.scope[0].answers_to("U"));
        assert!(!c.scope[0].answers_to("users"));
        assert!(c.scope[1].answers_to("items"));
        assert_eq!(c.scope[0].schema(), Some("public"));
    }

    #[test]
    fn ctes_and_derived_tables() {
        let c = at(
            "with recent as (select * from orders), b as materialized (select 2) select | from recent r, (select 1) d",
        );
        assert_eq!(c.ctes, ["recent", "b"]);
        assert!(c.scope.iter().any(|t| t.parts == ["orders"]));
        assert!(
            c.scope
                .iter()
                .any(|t| t.parts == ["recent"] && t.alias.as_deref() == Some("r"))
        );
        assert!(
            c.scope
                .iter()
                .any(|t| t.parts.is_empty() && t.alias.as_deref() == Some("d"))
        );
    }

    #[test]
    fn statement_region_stops_at_semicolons() {
        let c = at("select * from a x; select x.| from b y");
        assert_eq!(c.qualifier, ["x"]);
        assert!(c.scope.iter().all(|t| t.parts != ["a"]), "{:?}", c.scope);
    }
}
