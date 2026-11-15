//! MySQL / MariaDB dialect: backtick quoting, `?` placeholders, literals, explain, and lexical
//! rules kept in sync with the session's `sql_mode` (docs/08, ADR-0004).
//!
//! The classifier and the splitter of `ninesql-driver-api` follow [`LexicalRules::MYSQL`] with
//! the session's `NO_BACKSLASH_ESCAPES`. `ANSI_QUOTES` has no switch in [`LexicalRules`]: with it
//! on, `"…"` is an identifier where a backslash is an ordinary character, while the shared lexer
//! would still apply backslash escapes inside it. This dialect rewrites each `"…"` identifier
//! into a backtick identifier of the same length before splitting or classifying, so both see
//! the same token boundaries as the server.
//!
//! [`split_script`](Dialect::split_script) follows the mysql client's `DELIMITER` directive
//! (P3-D): `DELIMITER //` at the start of a statement changes the statement terminator until
//! the next `DELIMITER` (the directive runs to the end of its line and is never sent). With a
//! custom delimiter, the text between two delimiters is split again with
//! [`split_statements`](Dialect::split_statements), so a statement item is always one statement
//! for the single-statement check. A stored-program definition or a MariaDB `BEGIN NOT ATOMIC
//! … END` block is one statement in both. Other mysql client commands (a statement starting
//! with `\`) are refused.
//!
//! Parameters ([`bind_params`]) are inlined client-side as literals that cannot leave their
//! token whatever the session's modes: hexadecimal literals (`X'…'`, converted with `CONVERT(…
//! USING utf8mb4)` for text), digits, `DATE` / `TIMESTAMP` literals. Placeholders are found with
//! the same scanner, so a `?` inside a string, an identifier or a comment is never replaced.

use std::borrow::Cow;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use ninesql_driver_api::dialect::{classify_with_rules, split_with_rules};
use ninesql_driver_api::{
    Dialect, DriverError, DriverResult, ExplainFormat, LexicalRules, Param, RowChange, RowEdit,
    ScriptItem, ScriptItemKind, ScriptRefusal, ScriptState, Statement, StatementClass,
    StatementSpan, TextPosition, script_items, script_items_from,
};

/// The session's lexical modes, shared between the session and its dialect.
#[derive(Debug)]
pub(crate) struct Modes {
    pub(crate) no_backslash_escapes: AtomicBool,
    pub(crate) ansi_quotes: AtomicBool,
}

impl Default for Modes {
    fn default() -> Self {
        Self {
            no_backslash_escapes: AtomicBool::new(false),
            ansi_quotes: AtomicBool::new(false),
        }
    }
}

impl Modes {
    /// Applies an `@@sql_mode` value.
    pub(crate) fn set_sql_mode(&self, sql_mode: &str) {
        let has = |m: &str| {
            sql_mode
                .split(',')
                .any(|x| x.trim().eq_ignore_ascii_case(m))
        };
        self.no_backslash_escapes
            .store(has("NO_BACKSLASH_ESCAPES"), Ordering::SeqCst);
        self.ansi_quotes.store(has("ANSI_QUOTES"), Ordering::SeqCst);
    }
}

/// The MySQL / MariaDB dialect.
#[derive(Clone, Debug)]
pub struct MyDialect {
    modes: Arc<Modes>,
    /// `EXPLAIN ANALYZE` is offered (MySQL ≥ 8.0.18; MariaDB spells it `ANALYZE`).
    explain_analyze: bool,
}

impl MyDialect {
    /// The dialect in the default `sql_mode` (backslash escapes on, `"…"` strings).
    #[must_use]
    pub fn mysql() -> Self {
        Self {
            modes: Arc::default(),
            explain_analyze: true,
        }
    }

    /// The MariaDB dialect (no `EXPLAIN ANALYZE`).
    #[must_use]
    pub fn mariadb() -> Self {
        Self {
            modes: Arc::default(),
            explain_analyze: false,
        }
    }

    /// The same dialect with the lexical modes of an `@@sql_mode` value
    /// (`NO_BACKSLASH_ESCAPES`, `ANSI_QUOTES`), not tied to any session (tests, offline
    /// splitting).
    #[must_use]
    pub fn with_sql_mode(self, sql_mode: &str) -> Self {
        let modes = Modes::default();
        modes.set_sql_mode(sql_mode);
        Self {
            modes: Arc::new(modes),
            explain_analyze: self.explain_analyze,
        }
    }

    pub(crate) fn tracking(modes: Arc<Modes>, explain_analyze: bool) -> Self {
        Self {
            modes,
            explain_analyze,
        }
    }

    fn scan_rules(&self) -> ScanRules {
        ScanRules {
            backslash: !self.modes.no_backslash_escapes.load(Ordering::SeqCst),
            ansi_quotes: self.modes.ansi_quotes.load(Ordering::SeqCst),
        }
    }

    /// The text as the shared lexer must see it (see the module documentation).
    fn normalized<'a>(&self, text: &'a str) -> Cow<'a, str> {
        let rules = self.scan_rules();
        if !rules.ansi_quotes || !text.contains('"') {
            return Cow::Borrowed(text);
        }
        let mut out = text.as_bytes().to_vec();
        for seg in scan(text, rules) {
            if seg.kind == Seg::DoubleQuotedIdent {
                let (s, e) = (seg.start, seg.end);
                for b in &mut out[s + 1..e.saturating_sub(1).max(s + 1)] {
                    if *b == b'`' {
                        *b = b'_';
                    }
                }
                out[s] = b'`';
                if e > s + 1 && text.as_bytes()[e - 1] == b'"' {
                    out[e - 1] = b'`';
                }
            }
        }
        // Only ASCII bytes were swapped for ASCII bytes: still valid UTF-8, same offsets.
        Cow::Owned(String::from_utf8(out).unwrap_or_else(|_| text.to_owned()))
    }

    /// Inlines `params` into `text` (see [`bind_params`]).
    pub(crate) fn bind(&self, text: &str, params: &[Param]) -> DriverResult<String> {
        bind_params(text, params, self.scan_rules())
    }
}

impl Dialect for MyDialect {
    fn lexical_rules(&self) -> LexicalRules {
        // With `ANSI_QUOTES`, `"…"` is an identifier without backslash escapes (the text is
        // also normalized, see the module documentation); the rules then differ, so a caller
        // comparing `lexical_rules()` sees every mode change.
        LexicalRules::MYSQL
            .with_backslash_escapes(!self.modes.no_backslash_escapes.load(Ordering::SeqCst))
            .with_double_quote_backslash(!self.modes.ansi_quotes.load(Ordering::SeqCst))
    }

    fn quote_identifier(&self, ident: &str) -> String {
        let mut out = String::with_capacity(ident.len() + 2);
        out.push('`');
        for c in ident.chars() {
            if c == '`' {
                out.push('`');
            }
            out.push(c);
        }
        out.push('`');
        out
    }

    fn format_literal(&self, value: &Param) -> String {
        literal(value).unwrap_or_else(|_| "NULL".to_owned())
    }

    fn split_statements(&self, script: &str) -> Vec<StatementSpan> {
        let spans = split_with_rules(&self.normalized(script), self.lexical_rules());
        merge_compound(script, spans, self.scan_rules())
    }

    fn classify(&self, text: &str) -> StatementClass {
        classify_with_rules(&self.normalized(text), self.lexical_rules())
    }

    fn split_script(&self, script: &str) -> Vec<ScriptItem> {
        let (pieces, _) = split_with_delimiters(self, script, self.scan_rules(), 0, None, false);
        script_items(script, pieces)
    }

    fn split_script_ahead(
        &self,
        script: &str,
        from: TextPosition,
        state: &ScriptState,
    ) -> Vec<ScriptItem> {
        if from.offset > script.len() || !script.is_char_boundary(from.offset) {
            return self.split_script_from(script, from.offset);
        }
        // The caller passes the delimiter in force: nothing before `from` is read again. One
        // that a `DELIMITER` directive refuses can never be in force: the rest is refused
        // (fails closed; `;` is the directive's own spelling of the default).
        let delim = match state.delimiter.as_deref() {
            None | Some(";") => None,
            Some(d) if delimiter_accepted(d) => Some(d.to_owned()),
            Some(_) => {
                let p = skip_blanks(script, from.offset);
                if p >= script.len() {
                    return Vec::new();
                }
                return script_items_from(
                    script,
                    from,
                    [(
                        trimmed(script, p, script.len()),
                        ScriptItemKind::Refused(ScriptRefusal::InvalidDelimiter),
                    )],
                );
            }
        };
        let (pieces, _) =
            split_with_delimiters(self, script, self.scan_rules(), from.offset, delim, true);
        script_items_from(script, from, pieces)
    }

    fn script_state_after(
        &self,
        script: &str,
        item: &ScriptItem,
        state: &ScriptState,
    ) -> ScriptState {
        if item.kind != ScriptItemKind::ClientDirective {
            return state.clone();
        }
        match delimiter_directive(script, item.span.start) {
            Some((_, Some(d))) => ScriptState::with_delimiter((d != ";").then_some(d)),
            _ => state.clone(),
        }
    }

    fn split_script_from(&self, script: &str, from: usize) -> Vec<ScriptItem> {
        let mut from = from.min(script.len());
        while !script.is_char_boundary(from) {
            from -= 1;
        }
        let rules = self.scan_rules();
        // The `DELIMITER` in force at `from`, rebuilt from the text before it.
        let (_, delim) = split_with_delimiters(self, &script[..from], rules, 0, None, false);
        let (pieces, _) = split_with_delimiters(self, script, rules, from, delim, false);
        script_items(script, pieces)
    }

    fn explain(&self, query: &str, format: ExplainFormat) -> Option<String> {
        let q = query.trim().trim_end_matches(';');
        Some(match format {
            ExplainFormat::Text => format!("EXPLAIN {q}"),
            ExplainFormat::Json => format!("EXPLAIN FORMAT=JSON {q}"),
            ExplainFormat::Analyze if self.explain_analyze => format!("EXPLAIN ANALYZE {q}"),
            _ => return None,
        })
    }

    fn render_edit(&self, edit: &RowEdit) -> DriverResult<Statement> {
        if let RowChange::Insert { values } = &edit.change
            && values.is_empty()
        {
            // MySQL has no `DEFAULT VALUES`.
            if edit.table.is_empty() {
                return Err(DriverError::InvalidParams("edit without a table".into()));
            }
            return Ok(Statement::sql(format!(
                "INSERT INTO {} () VALUES ()",
                self.qualified_name(&edit.table)
            )));
        }
        Base(self).render_edit(edit)
    }
}

/// The trait defaults with this dialect's quoting (for `render_edit`).
struct Base<'a>(&'a MyDialect);

impl Dialect for Base<'_> {
    fn lexical_rules(&self) -> LexicalRules {
        self.0.lexical_rules()
    }

    fn quote_identifier(&self, ident: &str) -> String {
        self.0.quote_identifier(ident)
    }
}

// ---------------------------------------------------------------------------------------------
// Scanner

#[derive(Clone, Copy, Debug)]
pub(crate) struct ScanRules {
    pub(crate) backslash: bool,
    pub(crate) ansi_quotes: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Seg {
    /// Code, including the inside of executable comments.
    Code,
    /// String literal, backtick identifier, or `"…"` string.
    Quoted,
    /// `"…"` identifier (`ANSI_QUOTES`).
    DoubleQuotedIdent,
    Comment,
}

#[derive(Clone, Copy, Debug)]
struct Segment {
    kind: Seg,
    start: usize,
    end: usize,
}

fn skip_quoted(b: &[u8], start: usize, quote: u8, backslash: bool) -> usize {
    let mut i = start + 1;
    while i < b.len() {
        if backslash && b[i] == b'\\' {
            i += 2;
            continue;
        }
        if b[i] == quote {
            if b.get(i + 1) == Some(&quote) {
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    b.len()
}

fn executable_opener(b: &[u8], i: usize) -> Option<usize> {
    let rest = &b[i..];
    let base = if rest.starts_with(b"/*!") {
        3
    } else if rest.starts_with(b"/*M!") {
        4
    } else {
        return None;
    };
    Some(
        base + rest[base..]
            .iter()
            .take_while(|c| c.is_ascii_digit())
            .count(),
    )
}

/// Splits `text` into code, quoted and comment segments with MySQL's lexical rules (the same
/// rules as `LexicalRules::MYSQL` of the shared lexer, plus `ANSI_QUOTES`).
fn scan(text: &str, rules: ScanRules) -> Vec<Segment> {
    let mut out = Vec::new();
    scan_with(text, rules, |seg| {
        out.push(seg);
        true
    });
    out
}

/// [`scan`], handing each segment to `f` in order and stopping as soon as `f` returns `false`.
fn scan_with(text: &str, rules: ScanRules, mut f: impl FnMut(Segment) -> bool) {
    let b = text.as_bytes();
    let mut i = 0;
    let mut code_start = 0;
    let mut in_exec = false;
    // Hands the code before `s`, then the segment `s..e`, to `f`; `false` stops the scan.
    let mut push = |code_start: &mut usize, kind, s, e| -> bool {
        if *code_start < s
            && !f(Segment {
                kind: Seg::Code,
                start: *code_start,
                end: s,
            })
        {
            return false;
        }
        *code_start = e;
        // `s == e`: only the code before `s` is handed over.
        s == e
            || f(Segment {
                kind,
                start: s,
                end: e,
            })
    };
    while i < b.len() {
        let c = b[i];
        let next = b.get(i + 1).copied();
        if in_exec && c == b'*' && next == Some(b'/') {
            in_exec = false;
            if !push(&mut code_start, Seg::Comment, i, i + 2) {
                return;
            }
            i += 2;
        } else if c == b'#'
            || (c == b'-'
                && next == Some(b'-')
                && b.get(i + 2).is_none_or(|x| *x <= b' ' || *x == 0x7f))
        {
            let e = b[i..]
                .iter()
                .position(|x| *x == b'\n')
                .map_or(b.len(), |p| i + p);
            if !push(&mut code_start, Seg::Comment, i, e) {
                return;
            }
            i = e;
        } else if c == b'/' && next == Some(b'*') {
            if let Some(len) = executable_opener(b, i)
                && !in_exec
            {
                in_exec = true;
                if !push(&mut code_start, Seg::Comment, i, i + len) {
                    return;
                }
                i += len;
                continue;
            }
            let e = (i + 2..b.len().saturating_sub(1))
                .find(|&j| b[j] == b'*' && b[j + 1] == b'/')
                .map_or(b.len(), |j| j + 2);
            if !push(&mut code_start, Seg::Comment, i, e) {
                return;
            }
            i = e;
        } else if c == b'\'' || c == b'`' || c == b'"' {
            let ident_dq = c == b'"' && rules.ansi_quotes;
            let backslash = rules.backslash && c != b'`' && !ident_dq;
            let e = skip_quoted(b, i, c, backslash).min(b.len());
            let kind = if ident_dq {
                Seg::DoubleQuotedIdent
            } else {
                Seg::Quoted
            };
            if !push(&mut code_start, kind, i, e) {
                return;
            }
            i = e;
        } else {
            // Code is handed over line by line, so a caller that stops early (the delimiter
            // search) never waits for the end of a script without quotes or comments. No word
            // and no delimiter spans a line feed.
            if c == b'\n' && !push(&mut code_start, Seg::Code, i + 1, i + 1) {
                return;
            }
            i += 1;
        }
    }
    if code_start < b.len() {
        f(Segment {
            kind: Seg::Code,
            start: code_start,
            end: b.len(),
        });
    }
}

/// Upper-cased words of the code segments of `text` (outside strings, identifiers, comments).
fn code_words(text: &str, rules: ScanRules) -> Vec<String> {
    let mut words = Vec::new();
    for seg in scan(text, rules) {
        if seg.kind != Seg::Code {
            continue;
        }
        let code = &text[seg.start..seg.end];
        for w in code.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$')) {
            if !w.is_empty() {
                words.push(w.to_ascii_uppercase());
            }
        }
    }
    words
}

/// `CREATE … PROCEDURE | FUNCTION | TRIGGER | EVENT` with a `BEGIN` after it, or a MariaDB
/// anonymous block `BEGIN NOT ATOMIC …`.
/// `CREATE [OR REPLACE] [DEFINER = user] [ALGORITHM = …] [SQL SECURITY …] [AGGREGATE]
/// PROCEDURE | FUNCTION | TRIGGER | EVENT … BEGIN`, the object kind right after those clauses
/// (security review L3), or a MariaDB anonymous block `BEGIN NOT ATOMIC …`. The definer is
/// skipped up to the next clause keyword, so dotted hosts (`root@127.0.0.1`) are understood.
fn is_stored_program(words: &[String]) -> bool {
    if words.len() >= 3 && words[0] == "BEGIN" && words[1] == "NOT" && words[2] == "ATOMIC" {
        return true;
    }
    if words.first().map(String::as_str) != Some("CREATE") {
        return false;
    }
    let mut i = 1;
    let at = |i: usize| words.get(i).map(String::as_str);
    if at(i) == Some("OR") && at(i + 1) == Some("REPLACE") {
        i += 2;
    }
    if at(i) == Some("ALGORITHM") {
        i += 2;
    }
    if at(i) == Some("DEFINER") {
        i += 1;
        // The definer as code words: `CURRENT_USER`, or a user and a host that may hold dots
        // (`root@127.0.0.1` is the words ROOT, 127, 0, 0, 1; quoted parts are no words). Skip
        // them up to the next clause or object keyword (re-review M2); a view is no program.
        let stop = |w: &str| {
            matches!(
                w,
                "ALGORITHM"
                    | "SQL"
                    | "AGGREGATE"
                    | "PROCEDURE"
                    | "FUNCTION"
                    | "TRIGGER"
                    | "EVENT"
                    | "VIEW"
            )
        };
        let mut skipped = 0;
        while let Some(w) = at(i) {
            if stop(w) || skipped == 16 {
                break;
            }
            i += 1;
            skipped += 1;
        }
    }
    if at(i) == Some("ALGORITHM") {
        i += 2;
    }
    if at(i) == Some("SQL") && at(i + 1) == Some("SECURITY") {
        i += 3;
    }
    if at(i) == Some("AGGREGATE") {
        i += 1;
    }
    matches!(at(i), Some("PROCEDURE" | "FUNCTION" | "TRIGGER" | "EVENT"))
        && words[i + 1..].iter().any(|w| w == "BEGIN")
}

/// Counts open `BEGIN … END` / `CASE … END` blocks over words fed in order, with one word of
/// look-ahead (`END IF`, `END LOOP`, `END WHILE`, `END REPEAT`, `END FOR` close blocks whose
/// openers are not counted), so a growing definition is counted once, not rescanned
/// (security review M4).
#[derive(Default)]
struct BlockCounter {
    depth: i64,
    prev_end: bool,
    pending_end: bool,
}

impl BlockCounter {
    fn feed(&mut self, w: &str) {
        if self.pending_end {
            if !matches!(w, "IF" | "LOOP" | "WHILE" | "REPEAT" | "FOR") {
                self.depth -= 1;
            }
            self.pending_end = false;
        }
        match w {
            "BEGIN" => self.depth += 1,
            "CASE" if !self.prev_end => self.depth += 1,
            "END" => self.pending_end = true,
            _ => {}
        }
        self.prev_end = w == "END";
    }

    /// Open blocks now (a trailing `END` closes one).
    fn open(&self) -> i64 {
        self.depth - i64::from(self.pending_end)
    }
}

/// Joins the spans of a stored-program definition whose `BEGIN … END` body holds `;`: the
/// server receives it as one statement (one protocol call, no multi-statements), so it is one
/// statement for the single-statement check too. Text after the closing `END` stays a separate
/// statement (and is refused as such).
fn merge_compound(text: &str, spans: Vec<StatementSpan>, rules: ScanRules) -> Vec<StatementSpan> {
    let mut out = Vec::with_capacity(spans.len());
    let mut i = 0;
    while i < spans.len() {
        let mut span = spans[i];
        let words = code_words(span.text(text), rules);
        if is_stored_program(&words) {
            let mut blocks = BlockCounter::default();
            for w in &words {
                blocks.feed(w);
            }
            while i + 1 < spans.len() && blocks.open() > 0 {
                i += 1;
                for w in code_words(spans[i].text(text), rules) {
                    blocks.feed(&w);
                }
                span.end = spans[i].end;
            }
        }
        out.push(span);
        i += 1;
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Scripts: the mysql client's DELIMITER

/// Offset of the first byte at or after `p` that is not blank or inside a comment (`#`, `-- `,
/// `/* */`; an executable comment `/*! … */` is code).
fn skip_blanks(text: &str, mut p: usize) -> usize {
    let b = text.as_bytes();
    loop {
        while p < b.len() && b[p].is_ascii_whitespace() {
            p += 1;
        }
        let rest = &b[p..];
        let dash = rest.starts_with(b"--") && rest.get(2).is_none_or(|x| *x <= b' ' || *x == 0x7f);
        if rest.starts_with(b"#") || dash {
            p = b[p..]
                .iter()
                .position(|x| *x == b'\n')
                .map_or(b.len(), |q| p + q);
        } else if rest.starts_with(b"/*") && executable_opener(b, p).is_none() {
            p = (p + 2..b.len().saturating_sub(1))
                .find(|&j| b[j] == b'*' && b[j + 1] == b'/')
                .map_or(b.len(), |j| j + 2);
        } else {
            return p;
        }
    }
}

/// End of the line holding `from` (its `\n`, or the end of the text).
fn line_end(text: &str, from: usize) -> usize {
    text[from..].find('\n').map_or(text.len(), |q| from + q)
}

/// `start..end` without trailing whitespace.
fn trimmed(text: &str, start: usize, end: usize) -> StatementSpan {
    StatementSpan {
        start,
        end: start + text[start..end].trim_end().len(),
    }
}

/// When a `DELIMITER` directive starts at `p`: the end of its line and the new delimiter
/// (`None` when it is missing or refused).
fn delimiter_directive(text: &str, p: usize) -> Option<(usize, Option<String>)> {
    const WORD: &str = "delimiter";
    let rest = text.get(p..)?;
    let head = rest.get(..WORD.len())?;
    if !head.eq_ignore_ascii_case(WORD) {
        return None;
    }
    let after = rest.as_bytes().get(WORD.len()).copied();
    if after.is_some_and(|c| !(c == b' ' || c == b'\t' || c == b'\r' || c == b'\n')) {
        return None; // `delimiters`, `delimiter_x`: not the directive
    }
    let end = line_end(text, p);
    let arg = text[p + WORD.len()..end].trim();
    // The mysql client takes the first word (or a quoted string, which NineSQL refuses).
    let delim = arg.split_whitespace().next().unwrap_or("");
    Some((end, delimiter_accepted(delim).then(|| delim.to_owned())))
}

/// Whether a `DELIMITER` directive accepts `delim`: non-empty, one word (no whitespace), no
/// backslash or quote, no comment opener. The [`ScriptState`] delimiter a caller passes to
/// [`Dialect::split_script_ahead`] is held to the same rule (security review: an empty one
/// used to panic, and a delimiter the directive refuses could never be in force).
fn delimiter_accepted(delim: &str) -> bool {
    !(delim.is_empty()
        || delim.contains(char::is_whitespace)
        || delim.contains(['\\', '\'', '"', '`'])
        || delim.starts_with('#')
        || delim.starts_with("--")
        || delim.contains("/*"))
}

/// Offset of the first occurrence of `delim` (ASCII case-insensitive) in code at or after
/// `from`: outside strings, quoted identifiers, comments and executable comments.
/// `None` for an empty `delim` (never in force: [`delimiter_accepted`]) or a `from` that is not
/// a char boundary of `text`.
fn find_delimiter(text: &str, from: usize, delim: &str, rules: ScanRules) -> Option<usize> {
    if delim.is_empty() {
        return None;
    }
    let rest = text.get(from..)?;
    let mut in_exec = false;
    let mut found = None;
    // Stops at the first match: a script under a custom delimiter is scanned once.
    scan_with(rest, rules, |seg| {
        let raw = &rest[seg.start..seg.end];
        if seg.kind == Seg::Comment {
            if executable_opener(raw.as_bytes(), 0).is_some() && !raw.ends_with("*/") {
                in_exec = true;
            } else if in_exec && raw == "*/" {
                in_exec = false;
            }
        }
        // Inside an executable comment the delimiter does not end the statement.
        if seg.kind != Seg::Code || in_exec {
            return true;
        }
        let code = &rest.as_bytes()[seg.start..seg.end];
        let d = delim.as_bytes();
        match code
            .windows(d.len())
            .position(|w| w.eq_ignore_ascii_case(d))
        {
            Some(q) => {
                found = Some(from + seg.start + q);
                false
            }
            None => true,
        }
    });
    found
}

/// Bytes of script split per step in the default-delimiter mode (doubled while no statement
/// ends inside the window).
const SCRIPT_WINDOW: usize = 64 * 1024;

/// Bytes before the end of a window whose lexing may depend on text past it: the lexers look
/// at most four bytes ahead (`/*M!`, `-- `); the rest is slack.
const LOOKAHEAD_MARGIN: usize = 64;

/// Smallest split window.
const MIN_WINDOW: usize = 512;

/// The next window: twice what the last step consumed, between [`MIN_WINDOW`] and
/// [`SCRIPT_WINDOW`] (security review M4: a directive every few bytes costs a few hundred
/// bytes of lexing, not a full window).
fn next_window(consumed: usize) -> usize {
    consumed.saturating_mul(2).clamp(MIN_WINDOW, SCRIPT_WINDOW)
}

/// The pieces of `script` from byte `start`, with `delim` in force there (`None`: `;`), and
/// the delimiter in force at the end. With `lazy`, only the first step's worth (at least one
/// piece unless only blanks and comments are left).
fn split_with_delimiters(
    d: &MyDialect,
    script: &str,
    rules: ScanRules,
    start: usize,
    delim: Option<String>,
    lazy: bool,
) -> (Vec<(StatementSpan, ScriptItemKind)>, Option<String>) {
    let mut pieces: Vec<(StatementSpan, ScriptItemKind)> = Vec::new();
    let mut delim = delim;
    let mut base = start;
    let mut window = MIN_WINDOW;
    let shifted = |s: StatementSpan, by: usize| StatementSpan {
        start: s.start + by,
        end: s.end + by,
    };
    'outer: loop {
        if lazy && !pieces.is_empty() {
            break;
        }
        let p = skip_blanks(script, base);
        if p >= script.len() {
            break;
        }
        if let Some((end, new)) = delimiter_directive(script, p) {
            let kind = match new {
                Some(n) => {
                    delim = (n != ";").then_some(n);
                    ScriptItemKind::ClientDirective
                }
                None => ScriptItemKind::Refused(ScriptRefusal::InvalidDelimiter),
            };
            pieces.push((trimmed(script, p, end), kind));
            base = end;
            continue;
        }
        if script.as_bytes()[p] == b'\\' {
            let end = line_end(script, p);
            pieces.push((
                trimmed(script, p, end),
                ScriptItemKind::Refused(ScriptRefusal::MysqlClientCommand),
            ));
            base = end;
            continue;
        }
        match &delim {
            Some(dl) if lazy => {
                // One window of the rest (security review L2: splitting the whole block up to
                // the next delimiter at every lazy step made a long block re-split after each
                // statement quadratic). The lexers look at most a few bytes ahead, so text
                // more than `LOOKAHEAD_MARGIN` bytes before the cut lexes as in the whole
                // script: a delimiter found there is the first one of the whole rest, and when
                // none is, the block runs at least to `cut - margin`, so the statements it ends
                // with a `;` before that point are those of the whole block.
                let mut cut = p.saturating_add(window).min(script.len());
                while !script.is_char_boundary(cut) {
                    cut += 1;
                }
                let complete = cut == script.len();
                let safe = if complete {
                    cut
                } else {
                    cut.saturating_sub(LOOKAHEAD_MARGIN)
                };
                let found = find_delimiter(&script[..cut], p, dl, rules)
                    .filter(|q| q.saturating_add(dl.len()) <= safe);
                if let Some(end) = found {
                    for s in d.split_statements(&script[p..end]) {
                        pieces.push((shifted(s, p), ScriptItemKind::Statement));
                    }
                    base = end + dl.len();
                    continue;
                }
                if complete {
                    for s in d.split_statements(&script[p..]) {
                        pieces.push((shifted(s, p), ScriptItemKind::Statement));
                    }
                    break;
                }
                // No delimiter ends before `limit`: split up to it, the last statement (maybe
                // cut) left for the next step.
                let mut limit = safe.saturating_sub(dl.len());
                while !script.is_char_boundary(limit) {
                    limit -= 1;
                }
                let mut spans = if limit > p {
                    d.split_statements(&script[p..limit])
                } else {
                    Vec::new()
                };
                spans.pop();
                if spans.is_empty() {
                    window = window.saturating_mul(2);
                    continue;
                }
                for s in spans {
                    pieces.push((shifted(s, p), ScriptItemKind::Statement));
                }
            }
            Some(dl) => {
                let end = find_delimiter(script, p, dl, rules).unwrap_or(script.len());
                for s in d.split_statements(&script[p..end]) {
                    pieces.push((shifted(s, p), ScriptItemKind::Statement));
                }
                base = (end + dl.len()).min(script.len());
                if end >= script.len() {
                    break;
                }
            }
            None => {
                // Split a window of the rest: the statements it ends with a `;` are those of
                // the whole rest; the last one may be cut and is split again from its first
                // token (so a script with many `DELIMITER` blocks is not re-split to its end
                // after each). A `;` inside an executable comment is the one exception: the
                // piece after it is lexed again without that context, which can only split it
                // further (and the driver refuses a piece holding several statements).
                let mut end = (p + window).min(script.len());
                while !script.is_char_boundary(end) {
                    end += 1;
                }
                let complete = end == script.len();
                let mut spans = d.split_statements(&script[p..end]);
                let mut next = None;
                if !complete {
                    next = spans.pop().map(|s| s.start + p);
                    if spans.is_empty() {
                        window = window.saturating_mul(2);
                        continue;
                    }
                }
                for s in spans {
                    let s = shifted(s, p);
                    let first = s.start;
                    if delimiter_directive(script, first).is_some()
                        || script.as_bytes()[first] == b'\\'
                    {
                        base = first;
                        window = next_window(first - p);
                        continue 'outer;
                    }
                    pieces.push((s, ScriptItemKind::Statement));
                }
                match next {
                    Some(n) => {
                        window = next_window(n - p);
                        base = n;
                    }
                    None => break,
                }
            }
        }
    }
    (pieces, delim)
}

// ---------------------------------------------------------------------------------------------
// Literals and parameters

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789ABCDEF";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from(DIGITS[usize::from(b >> 4)]));
        s.push(char::from(DIGITS[usize::from(b & 0x0f)]));
    }
    s
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = u32::try_from(doy - (153 * mp + 2) / 5 + 1).unwrap_or(1);
    let m = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap_or(1);
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn is_strict_decimal(s: &str) -> bool {
    let s = s.strip_prefix('-').unwrap_or(s);
    let (int, frac) = s.split_once('.').unwrap_or((s, ""));
    !int.is_empty()
        && int.bytes().all(|b| b.is_ascii_digit())
        && frac.bytes().all(|b| b.is_ascii_digit())
}

/// A literal that stays one token whatever the session's `sql_mode` or character set: only
/// ASCII letters, digits, `-`, `.`, `:`, space, parentheses and quotes around hex digits or a
/// fixed date format. Fails for values MySQL cannot represent (NaN, infinities).
pub(crate) fn literal(value: &Param) -> DriverResult<String> {
    Ok(match value {
        Param::Null => "NULL".to_owned(),
        Param::Bool(b) => if *b { "TRUE" } else { "FALSE" }.to_owned(),
        Param::Int(i) if *i < 0 => format!("({i})"),
        Param::Int(i) => i.to_string(),
        Param::Float(f) if f.is_finite() => {
            let s = format!("{f:e}");
            if *f < 0.0 { format!("({s})") } else { s }
        }
        Param::Float(_) => {
            return Err(DriverError::InvalidParams(
                "MySQL has no NaN or infinite floating-point values".into(),
            ));
        }
        Param::Decimal(d) if is_strict_decimal(d) => {
            if d.starts_with('-') {
                format!("({d})")
            } else {
                d.clone()
            }
        }
        Param::Decimal(_) => {
            return Err(DriverError::InvalidParams(
                "a decimal parameter must be plain digits with an optional sign and point".into(),
            ));
        }
        Param::Text(s) | Param::Json(s) => {
            if s.is_empty() {
                "CONVERT('' USING utf8mb4)".to_owned()
            } else {
                format!("CONVERT(X'{}' USING utf8mb4)", hex(s.as_bytes()))
            }
        }
        Param::Bytes(b) => format!("X'{}'", hex(b)),
        Param::Date(days) => {
            let (y, m, d) = civil_from_days(i64::from(*days));
            format!("DATE '{y:04}-{m:02}-{d:02}'")
        }
        Param::Timestamp { micros, .. } => {
            let secs = micros.div_euclid(1_000_000);
            let frac = micros.rem_euclid(1_000_000);
            let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
            let sod = secs.rem_euclid(86_400);
            format!(
                "TIMESTAMP '{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}.{frac:06}'",
                sod / 3600,
                (sod % 3600) / 60,
                sod % 60
            )
        }
        _ => {
            return Err(DriverError::Unsupported(
                ninesql_driver_api::Feature::Parameters,
            ));
        }
    })
}

/// `true` when every parameter can be inlined. UTC timestamps are refused: MySQL `DATETIME` has
/// no time zone, and `TIMESTAMP` reads literals in the session time zone.
pub(crate) fn params_supported(params: &[Param]) -> bool {
    params.iter().all(|p| {
        matches!(
            p,
            Param::Null
                | Param::Bool(_)
                | Param::Int(_)
                | Param::Float(_)
                | Param::Decimal(_)
                | Param::Text(_)
                | Param::Json(_)
                | Param::Bytes(_)
                | Param::Date(_)
                | Param::Timestamp { utc: false, .. }
        )
    })
}

/// Replaces each `?` placeholder of `text` (outside strings, identifiers and comments) by the
/// literal of the next parameter, surrounded by spaces (so `-?` never becomes a `--` comment).
///
/// # Errors
/// [`DriverError::InvalidParams`] when the numbers of placeholders and parameters differ.
pub(crate) fn bind_params(text: &str, params: &[Param], rules: ScanRules) -> DriverResult<String> {
    if params.is_empty() {
        return Ok(text.to_owned());
    }
    let mut out = String::with_capacity(text.len() + params.len() * 16);
    let mut next = params.iter();
    let mut used = 0usize;
    for seg in scan(text, rules) {
        let s = &text[seg.start..seg.end];
        if seg.kind != Seg::Code {
            out.push_str(s);
            continue;
        }
        let mut last = 0;
        for (i, _) in s.match_indices('?') {
            out.push_str(&s[last..i]);
            let p = next.next().ok_or_else(|| {
                DriverError::InvalidParams(format!(
                    "the statement has more `?` placeholders than the {} parameters given",
                    params.len()
                ))
            })?;
            used += 1;
            out.push(' ');
            out.push_str(&literal(p)?);
            out.push(' ');
            last = i + 1;
        }
        out.push_str(&s[last..]);
    }
    if used != params.len() {
        return Err(DriverError::InvalidParams(format!(
            "the statement has {used} `?` placeholders for {} parameters",
            params.len()
        )));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ninesql_driver_api::{conformance, ensure_single_statement};
    use proptest::prelude::*;

    const DEFAULT: ScanRules = ScanRules {
        backslash: true,
        ansi_quotes: false,
    };

    #[test]
    fn quoting_roundtrip() {
        conformance::assert_dialect_quoting_roundtrip(
            &MyDialect::mysql(),
            conformance::ALIAS_TEMPLATE,
            conformance::IDENTIFIER_SAMPLES,
        );
        let modes = Arc::new(Modes::default());
        modes.set_sql_mode("ANSI_QUOTES,NO_BACKSLASH_ESCAPES");
        conformance::assert_dialect_quoting_roundtrip(
            &MyDialect::tracking(modes, true),
            conformance::ALIAS_TEMPLATE,
            conformance::IDENTIFIER_SAMPLES,
        );
    }

    #[test]
    fn sql_mode_drives_the_lexer() {
        let modes = Arc::new(Modes::default());
        let d = MyDialect::tracking(Arc::clone(&modes), true);
        // Default mode: `\'` escapes the quote, one statement holding a string.
        let sql = r"SELECT 'a\'; DELETE FROM t; -- '";
        assert_eq!(d.split_statements(sql).len(), 1);
        modes.set_sql_mode("STRICT_TRANS_TABLES,NO_BACKSLASH_ESCAPES");
        assert!(ensure_single_statement(&d, sql).is_err());
        // ANSI_QUOTES: `"a\"` is an identifier that ends at the second quote.
        modes.set_sql_mode("ANSI_QUOTES");
        let sql = r#"SELECT "a\"; DELETE FROM t; -- ""#;
        assert_eq!(d.split_statements(sql).len(), 2);
        assert_eq!(d.classify(sql), StatementClass::Destructive);
        modes.set_sql_mode("");
        assert_eq!(d.split_statements(sql).len(), 1);
        // A quoted setting name stays recognised after the rewrite.
        modes.set_sql_mode("ANSI_QUOTES");
        assert_eq!(
            d.classify("SET \"transaction_read_only\" = 0"),
            StatementClass::ChangesAccessMode
        );
    }

    #[test]
    fn stored_program_bodies_are_one_statement() {
        let d = MyDialect::mysql();
        let body = "CREATE DEFINER = `u`@`%` FUNCTION f() RETURNS INT DETERMINISTIC \
                    BEGIN DECLARE x INT; IF 1 THEN SET x = CASE WHEN 1 THEN 2 END; END IF; \
                    lbl: LOOP LEAVE lbl; END LOOP lbl; RETURN x; END";
        assert_eq!(d.split_statements(body).len(), 1);
        assert!(ensure_single_statement(&d, body).is_ok());
        // The riskiest inner statement wins: confirmation in production, refused read-only.
        assert_eq!(d.classify(body), StatementClass::Unknown);
        // Text after the closing END is another statement.
        let two = "CREATE PROCEDURE p() BEGIN SELECT 1; END; DROP TABLE t";
        assert_eq!(d.split_statements(two).len(), 2);
        // `begin` inside a string or an identifier does not open a block.
        let q = "CREATE PROCEDURE p() BEGIN SELECT 'begin', `begin`; END";
        assert_eq!(d.split_statements(q).len(), 1);
        // Not a stored program: plain splitting.
        assert_eq!(d.split_statements("BEGIN; SELECT 1; END").len(), 3);
    }

    #[test]
    fn params_are_inlined_as_inert_literals() {
        let p = vec![
            Param::Text("x' OR 1=1; -- \\".into()),
            Param::Int(-5),
            Param::Bytes(vec![0xde, 0xad]),
            Param::Null,
        ];
        let out = bind_params("SELECT ?, 1-?, ?, '?', `?`, ? -- ?", &p, DEFAULT).unwrap();
        assert_eq!(
            out,
            "SELECT  CONVERT(X'7827204F5220313D313B202D2D205C' USING utf8mb4) , 1- (-5) ,  \
             X'DEAD' , '?', `?`,  NULL  -- ?"
        );
        assert!(bind_params("SELECT ?", &[], DEFAULT).is_ok());
        assert!(bind_params("SELECT ?, ?", &[Param::Int(1)], DEFAULT).is_err());
        assert!(bind_params("SELECT 1", &[Param::Int(1)], DEFAULT).is_err());
        // An executable comment is code: its placeholder is bound.
        assert_eq!(
            bind_params("SELECT /*! ? */", &[Param::Int(1)], DEFAULT).unwrap(),
            "SELECT /*!  1  */"
        );
        assert!(literal(&Param::Float(f64::NAN)).is_err());
        assert_eq!(literal(&Param::Date(0)).unwrap(), "DATE '1970-01-01'");
        assert!(literal(&Param::Decimal("1; DROP".into())).is_err());
        assert!(!params_supported(&[Param::Timestamp {
            micros: 0,
            utc: true
        }]));
    }

    #[test]
    fn explain_and_edits() {
        let d = MyDialect::mysql();
        assert_eq!(
            d.explain("SELECT 1;", ExplainFormat::Json).as_deref(),
            Some("EXPLAIN FORMAT=JSON SELECT 1")
        );
        assert!(
            MyDialect::mariadb()
                .explain("SELECT 1", ExplainFormat::Analyze)
                .is_none()
        );
        let s = d
            .render_edit(&RowEdit {
                table: vec!["nine".into(), "t`x".into()],
                change: RowChange::Insert { values: vec![] },
            })
            .unwrap();
        assert_eq!(s.text, "INSERT INTO `nine`.`t``x` () VALUES ()");
        let s = d
            .render_edit(&RowEdit {
                table: vec!["t".into()],
                change: RowChange::Delete {
                    key: vec![("id".into(), Param::Int(1))],
                },
            })
            .unwrap();
        assert_eq!(s.text, "DELETE FROM `t` WHERE `id` = ?");
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(128))]

        /// Text parameters never change the statement's structure: the bound statement splits
        /// and classifies like the template.
        #[test]
        fn bound_text_never_escapes(value in "\\PC{0,40}", backslash in any::<bool>(), ansi in any::<bool>()) {
            let rules = ScanRules { backslash, ansi_quotes: ansi };
            let out = bind_params("SELECT * FROM t WHERE a = ?", &[Param::Text(value)], rules).unwrap();
            let d = MyDialect::mysql();
            prop_assert_eq!(d.split_statements(&out).len(), 1);
            prop_assert_eq!(d.classify(&out), StatementClass::Read);
        }

        /// The ANSI_QUOTES rewrite keeps offsets and char boundaries.
        #[test]
        fn rewrite_keeps_length(text in "\\PC{0,80}") {
            let modes = Arc::new(Modes::default());
            modes.set_sql_mode("ANSI_QUOTES");
            let d = MyDialect::tracking(modes, true);
            let n = d.normalized(&text);
            prop_assert_eq!(n.len(), text.len());
            for s in d.split_statements(&text) {
                prop_assert!(text.is_char_boundary(s.start) && text.is_char_boundary(s.end));
            }
        }
    }
}
