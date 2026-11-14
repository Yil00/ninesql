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
    Statement, StatementClass, StatementSpan,
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
        LexicalRules::MYSQL
            .with_backslash_escapes(!self.modes.no_backslash_escapes.load(Ordering::SeqCst))
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
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    let mut code_start = 0;
    let mut in_exec = false;
    let push = |out: &mut Vec<Segment>, code_start: &mut usize, kind, s, e| {
        if *code_start < s {
            out.push(Segment {
                kind: Seg::Code,
                start: *code_start,
                end: s,
            });
        }
        out.push(Segment {
            kind,
            start: s,
            end: e,
        });
        *code_start = e;
    };
    while i < b.len() {
        let c = b[i];
        let next = b.get(i + 1).copied();
        if in_exec && c == b'*' && next == Some(b'/') {
            in_exec = false;
            push(&mut out, &mut code_start, Seg::Comment, i, i + 2);
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
            push(&mut out, &mut code_start, Seg::Comment, i, e);
            i = e;
        } else if c == b'/' && next == Some(b'*') {
            if let Some(len) = executable_opener(b, i)
                && !in_exec
            {
                in_exec = true;
                push(&mut out, &mut code_start, Seg::Comment, i, i + len);
                i += len;
                continue;
            }
            let e = (i + 2..b.len().saturating_sub(1))
                .find(|&j| b[j] == b'*' && b[j + 1] == b'/')
                .map_or(b.len(), |j| j + 2);
            push(&mut out, &mut code_start, Seg::Comment, i, e);
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
            push(&mut out, &mut code_start, kind, i, e);
            i = e;
        } else {
            i += 1;
        }
    }
    if code_start < b.len() {
        out.push(Segment {
            kind: Seg::Code,
            start: code_start,
            end: b.len(),
        });
    }
    out
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

/// `CREATE … PROCEDURE | FUNCTION | TRIGGER | EVENT` with a `BEGIN` after it.
fn is_stored_program(words: &[String]) -> bool {
    if words.first().map(String::as_str) != Some("CREATE") {
        return false;
    }
    let begin = words.iter().position(|w| w == "BEGIN");
    let kind = words
        .iter()
        .position(|w| matches!(w.as_str(), "PROCEDURE" | "FUNCTION" | "TRIGGER" | "EVENT"));
    matches!((kind, begin), (Some(k), Some(b)) if k < b)
}

/// Open `BEGIN … END` / `CASE … END` blocks at the end of `words` (`END IF`, `END LOOP`,
/// `END WHILE`, `END REPEAT`, `END FOR` close blocks whose openers are not counted).
fn open_blocks(words: &[String]) -> i64 {
    let mut depth = 0i64;
    for (i, w) in words.iter().enumerate() {
        let prev = i.checked_sub(1).map(|p| words[p].as_str());
        let next = words.get(i + 1).map(String::as_str);
        match w.as_str() {
            "BEGIN" => depth += 1,
            "CASE" if prev != Some("END") => depth += 1,
            "END" if !matches!(next, Some("IF" | "LOOP" | "WHILE" | "REPEAT" | "FOR")) => {
                depth -= 1;
            }
            _ => {}
        }
    }
    depth
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
        if is_stored_program(&code_words(span.text(text), rules)) {
            while i + 1 < spans.len()
                && open_blocks(&code_words(&text[span.start..span.end], rules)) > 0
            {
                i += 1;
                span.end = spans[i].end;
            }
        }
        out.push(span);
        i += 1;
    }
    out
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
