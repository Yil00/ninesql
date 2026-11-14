//! SQL dialects (ADR-0004): identifier quoting, literals, statement splitting, paging, explain,
//! statement classification for the production guard (ADR-0007) and DML generation for edits.
//!
//! Every method has an ANSI default so a driver only overrides what its engine does differently.
//! The defaults are conservative: anything the classifier does not recognise is
//! [`StatementClass::Unknown`], which counts as destructive.
//!
//! # Security
//! The classifier is a guard rail, not the read-only mechanism (I3): engines enforce read-only
//! themselves. It must still never under-classify, so its lexer follows the engine's lexical
//! rules exactly ([`LexicalRules`]). Drivers track the session modes that change them and build
//! the rules from their current values:
//! - MySQL / MariaDB `sql_mode`: `NO_BACKSLASH_ESCAPES`, `ANSI_QUOTES` (`"…"` becomes an
//!   identifier);
//! - PostgreSQL `standard_conforming_strings`;
//! - the client encoding (PostgreSQL `client_encoding` via `ParameterStatus`, MySQL
//!   `character_set_client`): multi-byte encodings such as SJIS, Big5 or GBK can hide a quote
//!   behind a backslash, so drivers connect in UTF-8 and the classifier treats any non-UTF-8
//!   encoding change as [`StatementClass::ChangesAccessMode`].
//!
//! Drivers also send exactly one statement per protocol call (see [`ensure_single_statement`]).
//!
//! # Limits of the classifier (L4)
//! The classifier sees statement text, not what the server will run. A call to a function with
//! side effects is only recognised when the function is on a known list (`nextval`, `dblink_exec`,
//! replication-slot, backup and statistics-reset functions, large-object writers…): **any other
//! function, in particular user-defined functions and procedures called from a `SELECT`, is
//! classified [`StatementClass::Read`]** even if it writes. Consequences:
//! - read-only sessions stay safe: the engine-side mechanism (I3) rejects the write, and the
//!   driver verifies the access mode after every statement for liftable mechanisms;
//! - the production guard (ADR-0007 §3) does **not** ask for confirmation for such calls, and
//!   on an engine declared [`ReadOnlyMechanism::ClientSideOnly`](crate::ReadOnlyMechanism::ClientSideOnly)
//!   nothing stops them, which is why the UI recommends a read-only account there.
//!
//! PostgreSQL Unicode-escaped names (`U&"d\0061ta"`, `U&'…'`) are decoded before matching;
//! a malformed escape or a `UESCAPE` clause makes the statement [`StatementClass::Unknown`].

use crate::capabilities::ExplainFormat;
use crate::error::{DriverError, DriverResult};
use crate::statement::{Param, Statement};

/// Lexical rules used by the default splitter, classifier and literal formatter.
///
/// Build them from a preset ([`LexicalRules::ANSI`], [`POSTGRES`](Self::POSTGRES),
/// [`MYSQL`](Self::MYSQL), [`SQLITE`](Self::SQLITE)) and the `with_*` methods.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[non_exhaustive]
#[allow(clippy::struct_excessive_bools)]
pub struct LexicalRules {
    /// Backslash escapes inside every string literal (MySQL / MariaDB unless the session's
    /// `sql_mode` has `NO_BACKSLASH_ESCAPES`; PostgreSQL with `standard_conforming_strings = off`).
    pub backslash_escapes: bool,
    /// Backslash escapes inside `E'…'` strings (PostgreSQL family).
    pub e_string_backslash: bool,
    /// Block comments nest (PostgreSQL).
    pub nested_block_comments: bool,
    /// `$tag$ … $tag$` dollar-quoted strings (PostgreSQL family).
    pub dollar_quotes: bool,
    /// `#` starts a line comment (MySQL family).
    pub hash_comments: bool,
    /// `--` starts a comment only when followed by whitespace, a control character or the end
    /// of input (MySQL family): `1--1` is an expression.
    pub dash_comment_needs_space: bool,
    /// `/*! … */` and `/*M! … */` (optionally with a version number) are executed as code
    /// (MySQL / MariaDB executable comments).
    pub executable_comments: bool,
    /// `` ` `` quotes identifiers (MySQL family, SQLite, ClickHouse).
    pub backtick_identifiers: bool,
}

impl LexicalRules {
    /// ANSI SQL: `'…'` strings with `''`, `"…"` identifiers, `--` and `/* */` comments.
    pub const ANSI: Self = Self {
        backslash_escapes: false,
        e_string_backslash: false,
        nested_block_comments: false,
        dollar_quotes: false,
        hash_comments: false,
        dash_comment_needs_space: false,
        executable_comments: false,
        backtick_identifiers: false,
    };

    /// PostgreSQL family with `standard_conforming_strings = on` (the default since 9.1).
    pub const POSTGRES: Self = Self::ANSI
        .with_e_string_backslash(true)
        .with_nested_block_comments(true)
        .with_dollar_quotes(true);

    /// MySQL / MariaDB in the default `sql_mode` (backslash escapes on).
    pub const MYSQL: Self = Self::ANSI
        .with_backslash_escapes(true)
        .with_hash_comments(true)
        .with_dash_comment_needs_space(true)
        .with_executable_comments(true)
        .with_backtick_identifiers(true);

    /// SQLite.
    pub const SQLITE: Self = Self::ANSI.with_backtick_identifiers(true);

    /// Sets [`backslash_escapes`](Self::backslash_escapes).
    #[must_use]
    pub const fn with_backslash_escapes(mut self, on: bool) -> Self {
        self.backslash_escapes = on;
        self
    }

    /// Sets [`e_string_backslash`](Self::e_string_backslash).
    #[must_use]
    pub const fn with_e_string_backslash(mut self, on: bool) -> Self {
        self.e_string_backslash = on;
        self
    }

    /// Sets [`nested_block_comments`](Self::nested_block_comments).
    #[must_use]
    pub const fn with_nested_block_comments(mut self, on: bool) -> Self {
        self.nested_block_comments = on;
        self
    }

    /// Sets [`dollar_quotes`](Self::dollar_quotes).
    #[must_use]
    pub const fn with_dollar_quotes(mut self, on: bool) -> Self {
        self.dollar_quotes = on;
        self
    }

    /// Sets [`hash_comments`](Self::hash_comments).
    #[must_use]
    pub const fn with_hash_comments(mut self, on: bool) -> Self {
        self.hash_comments = on;
        self
    }

    /// Sets [`dash_comment_needs_space`](Self::dash_comment_needs_space).
    #[must_use]
    pub const fn with_dash_comment_needs_space(mut self, on: bool) -> Self {
        self.dash_comment_needs_space = on;
        self
    }

    /// Sets [`executable_comments`](Self::executable_comments).
    #[must_use]
    pub const fn with_executable_comments(mut self, on: bool) -> Self {
        self.executable_comments = on;
        self
    }

    /// Sets [`backtick_identifiers`](Self::backtick_identifiers).
    #[must_use]
    pub const fn with_backtick_identifiers(mut self, on: bool) -> Self {
        self.backtick_identifiers = on;
        self
    }
}

/// Byte range of one statement inside a script, excluding surrounding whitespace, leading
/// comments and the terminating `;`. Always on `char` boundaries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StatementSpan {
    /// Start offset (inclusive).
    pub start: usize,
    /// End offset (exclusive).
    pub end: usize,
}

impl StatementSpan {
    /// The statement text inside `script`.
    #[must_use]
    pub fn text<'a>(&self, script: &'a str) -> &'a str {
        script.get(self.start..self.end).unwrap_or("")
    }
}

/// Classification of a statement, ordered by increasing risk.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum StatementClass {
    /// Reads only (`SELECT`, `SHOW`, `VALUES`…).
    Read,
    /// Session settings that do not affect access mode (`SET search_path`, `USE`).
    Session,
    /// Transaction control (`BEGIN`, `COMMIT`, `ROLLBACK`, `SAVEPOINT`…).
    Transaction,
    /// Data modification with a filter (`INSERT`, `UPDATE … WHERE`, `DELETE … WHERE`, `MERGE`).
    Write,
    /// Non-destructive schema change (`CREATE`).
    Ddl,
    /// A session statement that may lift read-only mode (`SET … READ WRITE`,
    /// `PRAGMA query_only = OFF`). Refused in read-only sessions (I3).
    ChangesAccessMode,
    /// Destructive (`DROP`, `TRUNCATE`, `ALTER`, `GRANT`, `REVOKE`, `UPDATE` / `DELETE` without
    /// `WHERE`).
    Destructive,
    /// Not recognised. Treated as destructive (ADR-0007 §3).
    Unknown,
}

impl StatementClass {
    /// May modify data, schema or privileges. Refused client-side in read-only sessions in
    /// addition to the engine-side mechanism.
    #[must_use]
    pub fn is_write(self) -> bool {
        matches!(
            self,
            Self::Write | Self::Ddl | Self::ChangesAccessMode | Self::Destructive | Self::Unknown
        )
    }

    /// Needs explicit confirmation on connections whose type asks for it (ADR-0007 §3).
    #[must_use]
    pub fn needs_confirmation(self) -> bool {
        matches!(
            self,
            Self::ChangesAccessMode | Self::Destructive | Self::Unknown
        )
    }
}

/// A row change made in the grid, rendered to DML by [`Dialect::render_edit`].
#[derive(Clone, Debug, PartialEq)]
pub struct RowEdit {
    /// Qualified table name, unquoted parts (`["public", "users"]`).
    pub table: Vec<String>,
    /// The change.
    pub change: RowChange,
}

/// The change of a [`RowEdit`]. Values travel as bind parameters, never inlined.
#[derive(Clone, Debug, PartialEq)]
pub enum RowChange {
    /// Insert a row.
    Insert {
        /// Column / value pairs.
        values: Vec<(String, Param)>,
    },
    /// Update the row identified by `key`.
    Update {
        /// Unique key column / value pairs. Must not be empty.
        key: Vec<(String, Param)>,
        /// Column / new value pairs. Must not be empty.
        values: Vec<(String, Param)>,
    },
    /// Delete the row identified by `key`.
    Delete {
        /// Unique key column / value pairs. Must not be empty.
        key: Vec<(String, Param)>,
    },
}

/// Engine dialect. Obtained from [`Driver::dialect`](crate::Driver::dialect) or
/// [`Session::dialect`](crate::Session::dialect).
pub trait Dialect: Send + Sync {
    /// Lexical rules for the default implementations.
    fn lexical_rules(&self) -> LexicalRules {
        LexicalRules::default()
    }

    /// Quotes an identifier (ANSI: `"name"` with `"` doubled).
    fn quote_identifier(&self, ident: &str) -> String {
        let mut out = String::with_capacity(ident.len() + 2);
        out.push('"');
        for c in ident.chars() {
            if c == '"' {
                out.push('"');
            }
            out.push(c);
        }
        out.push('"');
        out
    }

    /// Quoted, dot-separated qualified name.
    fn qualified_name(&self, parts: &[String]) -> String {
        parts
            .iter()
            .map(|p| self.quote_identifier(p))
            .collect::<Vec<_>>()
            .join(".")
    }

    /// Formats a value as a literal for display / generated scripts. Execution paths use bind
    /// parameters instead.
    fn format_literal(&self, value: &Param) -> String {
        ansi_literal(value, self.lexical_rules().backslash_escapes)
    }

    /// Bind parameter placeholder for the 1-based `index` (ANSI / JDBC: `?`).
    fn placeholder(&self, index: usize) -> String {
        let _ = index;
        "?".to_owned()
    }

    /// Splits a script into statements on top-level `;`, honouring quotes and comments
    /// according to [`lexical_rules`](Self::lexical_rules).
    fn split_statements(&self, script: &str) -> Vec<StatementSpan> {
        split_with_rules(script, self.lexical_rules())
    }

    /// Wraps a query to fetch one page (ANSI: `SELECT * FROM (<q>) AS ninesql_page LIMIT n
    /// OFFSET m`). `None` when the engine has no paging syntax.
    fn page_query(&self, query: &str, limit: u64, offset: u64) -> Option<String> {
        let q = query.trim().trim_end_matches(';');
        Some(format!(
            "SELECT * FROM ({q}) AS ninesql_page LIMIT {limit} OFFSET {offset}"
        ))
    }

    /// Explain statement for `query` in `format`, for formats declared in
    /// [`Capabilities::explain`](crate::Capabilities::explain). Default: none.
    fn explain(&self, query: &str, format: ExplainFormat) -> Option<String> {
        let _ = (query, format);
        None
    }

    /// Classifies a statement (or a script: the riskiest statement wins). The default is a
    /// conservative keyword classifier; drivers with a real parser override it.
    ///
    /// Calls to functions with side effects that are not on the known list (user functions,
    /// procedures called from a `SELECT`) classify as [`StatementClass::Read`]: see the
    /// [module documentation](self#limits-of-the-classifier-l4).
    fn classify(&self, text: &str) -> StatementClass {
        classify_with_rules(text, self.lexical_rules())
    }

    /// Renders a grid edit as a parameterized DML statement.
    ///
    /// # Errors
    /// [`DriverError::InvalidParams`] when an update / delete has an empty key or an update has
    /// no values (it would touch every row).
    fn render_edit(&self, edit: &RowEdit) -> DriverResult<Statement> {
        render_edit_ansi(self, edit)
    }
}

/// The ANSI dialect, with every default.
#[derive(Clone, Copy, Debug, Default)]
pub struct AnsiDialect;

impl Dialect for AnsiDialect {}

// ---------------------------------------------------------------------------------------------
// Lexer

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TokKind {
    Word,
    /// String literal (`'…'`, `E'…'`, `$tag$…$tag$`).
    Str,
    /// Quoted identifier (`"…"`, `` `…` ``).
    Ident,
    Semicolon,
    Other,
}

#[derive(Clone, Copy, Debug)]
struct Tok {
    kind: TokKind,
    start: usize,
    end: usize,
}

struct Lexed {
    toks: Vec<Tok>,
    /// A `/*!` or `/*M!` comment was seen while executable comments were not enabled.
    executable_comment_as_comment: bool,
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80
}

/// Index just past the end of a quoted run starting at `start` (the opening quote).
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

/// If a dollar-quote tag starts at `start`, returns the tag length (including both `$`).
fn dollar_tag(bytes: &[u8], start: usize) -> Option<usize> {
    let mut i = start + 1;
    if let Some(&b) = bytes.get(i)
        && b.is_ascii_digit()
    {
        return None;
    }
    while let Some(&b) = bytes.get(i) {
        if b == b'$' {
            return Some(i + 1 - start);
        }
        if !(b.is_ascii_alphanumeric() || b == b'_') {
            return None;
        }
        i += 1;
    }
    None
}

fn find_sub(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from > hay.len() {
        return None;
    }
    hay[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

/// Length of an executable-comment opener (`/*!`, `/*M!`, plus version digits) at `i`.
fn executable_opener(bytes: &[u8], i: usize) -> Option<usize> {
    let rest = &bytes[i..];
    let base = if rest.starts_with(b"/*!") {
        3
    } else if rest.starts_with(b"/*M!") {
        4
    } else {
        return None;
    };
    let digits = rest[base..]
        .iter()
        .take_while(|b| b.is_ascii_digit())
        .count();
    Some(base + digits)
}

fn lex(text: &str, rules: LexicalRules) -> Lexed {
    let bytes = text.as_bytes();
    let mut toks = Vec::new();
    let mut executable_comment_as_comment = false;
    let mut in_executable = false;
    let mut i = 0;
    let push = |toks: &mut Vec<Tok>, kind, start, end| toks.push(Tok { kind, start, end });
    while i < bytes.len() {
        let b = bytes[i];
        let next = bytes.get(i + 1).copied();
        let dash_comment = b == b'-'
            && next == Some(b'-')
            && (!rules.dash_comment_needs_space
                || bytes.get(i + 2).is_none_or(|c| *c <= b' ' || *c == 0x7f));
        if b.is_ascii_whitespace() {
            i += 1;
        } else if in_executable && b == b'*' && next == Some(b'/') {
            // End of a `/*! … */` executable comment: its content was lexed as code.
            in_executable = false;
            i += 2;
        } else if dash_comment || rules.hash_comments && b == b'#' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
        } else if b == b'/' && next == Some(b'*') {
            if let Some(len) = executable_opener(bytes, i) {
                if rules.executable_comments && !in_executable {
                    in_executable = true;
                    i += len;
                    continue;
                }
                executable_comment_as_comment = true;
            }
            let mut depth = 1usize;
            i += 2;
            while i < bytes.len() && depth > 0 {
                if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                    depth -= 1;
                    i += 2;
                } else if rules.nested_block_comments
                    && bytes[i] == b'/'
                    && bytes.get(i + 1) == Some(&b'*')
                {
                    depth += 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            i = i.min(bytes.len());
        } else if rules.e_string_backslash && matches!(b, b'e' | b'E') && next == Some(b'\'') {
            let end = skip_quoted(bytes, i + 1, b'\'', true).min(bytes.len());
            push(&mut toks, TokKind::Str, i, end);
            i = end;
        } else if matches!(b, b'u' | b'U')
            && next == Some(b'&')
            && matches!(bytes.get(i + 2), Some(b'\'' | b'"'))
        {
            // `U&"…"` / `U&'…'` (SQL standard, PostgreSQL): one token, decoded by `locate`.
            // The quoted run is skipped exactly as without the prefix, so splitting is unchanged.
            let q = bytes[i + 2];
            let end = skip_quoted(bytes, i + 2, q, rules.backslash_escapes).min(bytes.len());
            let kind = if q == b'\'' {
                TokKind::Str
            } else {
                TokKind::Ident
            };
            push(&mut toks, kind, i, end);
            i = end;
        } else if b == b'\'' || b == b'"' || rules.backtick_identifiers && b == b'`' {
            let backslash = rules.backslash_escapes && b != b'`';
            let end = skip_quoted(bytes, i, b, backslash).min(bytes.len());
            let kind = if b == b'\'' {
                TokKind::Str
            } else {
                TokKind::Ident
            };
            push(&mut toks, kind, i, end);
            i = end;
        } else if rules.dollar_quotes && b == b'$' {
            if let Some(len) = dollar_tag(bytes, i) {
                let tag = &bytes[i..i + len];
                let end = find_sub(bytes, tag, i + len).map_or(bytes.len(), |p| p + len);
                push(&mut toks, TokKind::Str, i, end);
                i = end;
            } else {
                push(&mut toks, TokKind::Other, i, i + 1);
                i += 1;
            }
        } else if b == b';' {
            push(&mut toks, TokKind::Semicolon, i, i + 1);
            i += 1;
        } else if is_word_byte(b) {
            // With dollar quotes (PostgreSQL), `$` continues an identifier after its first
            // character (`a$q$` is one name); a dollar tag only starts at a token start.
            let start = i;
            while i < bytes.len()
                && (is_word_byte(bytes[i]) || rules.dollar_quotes && bytes[i] == b'$')
            {
                i += 1;
            }
            push(&mut toks, TokKind::Word, start, i);
        } else {
            push(&mut toks, TokKind::Other, i, i + 1);
            i += 1;
        }
    }
    Lexed {
        toks,
        executable_comment_as_comment,
    }
}

/// Splits `script` with the given lexical rules (the default of [`Dialect::split_statements`]).
#[must_use]
pub fn split_with_rules(script: &str, rules: LexicalRules) -> Vec<StatementSpan> {
    let mut spans = Vec::new();
    let mut current: Option<StatementSpan> = None;
    for t in lex(script, rules).toks {
        if t.kind == TokKind::Semicolon {
            if let Some(s) = current.take() {
                spans.push(s);
            }
        } else {
            match current.as_mut() {
                Some(s) => s.end = t.end,
                None => {
                    current = Some(StatementSpan {
                        start: t.start,
                        end: t.end,
                    });
                }
            }
        }
    }
    if let Some(s) = current {
        spans.push(s);
    }
    spans
}

/// Fails with [`DriverError::MultipleStatements`] when `text` contains more than one statement
/// for `dialect`.
///
/// Drivers call it in [`Session::execute`](crate::Session::execute) and send exactly one
/// statement per protocol call (PostgreSQL extended protocol, MySQL without
/// `CLIENT_MULTI_STATEMENTS`), so the statement the engine runs is the one that was classified.
///
/// # Errors
/// [`DriverError::MultipleStatements`] with the number of statements found.
pub fn ensure_single_statement<D: Dialect + ?Sized>(dialect: &D, text: &str) -> DriverResult<()> {
    let n = dialect.split_statements(text).len();
    if n > 1 {
        Err(DriverError::MultipleStatements(n))
    } else {
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// Classifier

/// One significant token of a statement, as seen by the classifier.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Item {
    /// Unquoted word, upper-cased.
    Word(String),
    /// Quoted identifier, unquoted and lower-cased.
    Ident(String),
    /// String literal, contents lower-cased (quotes and `E` prefix removed).
    Str(String),
    /// Punctuation.
    Punct(u8),
}

#[derive(Clone, Debug)]
struct Located {
    item: Item,
    /// Parenthesis depth (0 = top level).
    depth: u32,
}

/// Classifies `text` with the given lexical rules (the default of [`Dialect::classify`]).
#[must_use]
pub fn classify_with_rules(text: &str, rules: LexicalRules) -> StatementClass {
    let lexed = lex(text, rules);
    let mut worst = if lexed.executable_comment_as_comment {
        // The engine might execute what we skipped as a comment.
        Some(StatementClass::Unknown)
    } else {
        None
    };
    for stmt in lexed.toks.split(|t| t.kind == TokKind::Semicolon) {
        if stmt.is_empty() {
            continue;
        }
        let (items, opaque) = locate(text, stmt);
        let c = if opaque {
            // A name the classifier cannot decode could be anything.
            StatementClass::Unknown
        } else {
            classify_items(&items)
        };
        worst = Some(worst.map_or(c, |w| w.max(c)));
    }
    worst.unwrap_or(StatementClass::Read)
}

fn unquote_ident(raw: &str) -> String {
    let inner = raw.get(1..raw.len().saturating_sub(1)).unwrap_or("");
    inner
        .replace("\"\"", "\"")
        .replace("``", "`")
        .to_lowercase()
}

/// `U&"…"` / `U&'…'`: strips the prefix and the quotes, undoubles the quote and decodes
/// `\XXXX`, `\+XXXXXX` and `\\` (surrogate pairs included). `None` when the token is
/// unterminated or an escape is malformed.
fn decode_unicode_escaped(raw: &str) -> Option<String> {
    let body = raw.get(2..)?;
    let q = body.chars().next()?;
    if body.len() < 2 || !body.ends_with(q) {
        return None;
    }
    let doubled: String = [q, q].iter().collect();
    let inner = body[1..body.len() - 1].replace(&doubled, &q.to_string());
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars().peekable();
    let mut pending_high: Option<u32> = None;
    while let Some(c) = chars.next() {
        if c != '\\' {
            if pending_high.is_some() {
                return None;
            }
            out.push(c);
            continue;
        }
        let digits = match chars.peek() {
            Some('\\') => {
                chars.next();
                if pending_high.is_some() {
                    return None;
                }
                out.push('\\');
                continue;
            }
            Some('+') => {
                chars.next();
                6
            }
            _ => 4,
        };
        let hex: String = chars.by_ref().take(digits).collect();
        if hex.len() != digits || !hex.chars().all(|h| h.is_ascii_hexdigit()) {
            return None;
        }
        let cp = u32::from_str_radix(&hex, 16).ok()?;
        match (pending_high.take(), cp) {
            (None, 0xD800..=0xDBFF) => pending_high = Some(cp),
            (Some(high), 0xDC00..=0xDFFF) => {
                out.push(char::from_u32(
                    0x10000 + ((high - 0xD800) << 10) + (cp - 0xDC00),
                )?);
            }
            (Some(_), _) => return None,
            (None, _) => out.push(char::from_u32(cp)?),
        }
    }
    if pending_high.is_some() {
        return None;
    }
    Some(out)
}

fn has_unicode_prefix(raw: &str) -> bool {
    raw.len() > 2 && raw.as_bytes()[1] == b'&' && matches!(raw.as_bytes()[0], b'u' | b'U')
}

fn unquote_str(raw: &str) -> String {
    let body = raw.strip_prefix(['e', 'E']).unwrap_or(raw);
    let inner = if body.len() >= 2 && body.starts_with('\'') && body.ends_with('\'') {
        &body[1..body.len() - 1]
    } else {
        body
    };
    inner.to_lowercase()
}

/// Items of one statement, and whether a token could not be decoded (`opaque`).
fn locate(text: &str, toks: &[Tok]) -> (Vec<Located>, bool) {
    let mut depth = 0u32;
    let mut opaque = false;
    let mut out = Vec::with_capacity(toks.len());
    for t in toks {
        let raw = &text[t.start..t.end];
        let item = match t.kind {
            TokKind::Word => {
                if raw.eq_ignore_ascii_case("UESCAPE") {
                    // A custom escape character for `U&` names: not decoded, fail closed.
                    opaque = true;
                }
                Item::Word(raw.to_ascii_uppercase())
            }
            TokKind::Ident | TokKind::Str if has_unicode_prefix(raw) => {
                if let Some(decoded) = decode_unicode_escaped(raw) {
                    let lower = decoded.to_lowercase();
                    if t.kind == TokKind::Ident {
                        Item::Ident(lower)
                    } else {
                        Item::Str(lower)
                    }
                } else {
                    opaque = true;
                    Item::Punct(b'?')
                }
            }
            TokKind::Ident => Item::Ident(unquote_ident(raw)),
            TokKind::Str => Item::Str(unquote_str(raw)),
            TokKind::Other | TokKind::Semicolon => Item::Punct(raw.as_bytes()[0]),
        };
        if item == Item::Punct(b')') {
            depth = depth.saturating_sub(1);
        }
        out.push(Located {
            item: item.clone(),
            depth,
        });
        if item == Item::Punct(b'(') {
            depth += 1;
        }
    }
    (out, opaque)
}

/// Functions that change the session's access mode.
const ACCESS_MODE_FUNCTIONS: &[&str] = &["SET_CONFIG"];

/// Functions with side effects a `SELECT` can call. Unknown = needs confirmation, refused in
/// read-only sessions. Not exhaustive: see the module's "Limits of the classifier".
const SIDE_EFFECT_FUNCTIONS: &[&str] = &[
    "NEXTVAL",
    "SETVAL",
    "PG_TERMINATE_BACKEND",
    "PG_CANCEL_BACKEND",
    "PG_RELOAD_CONF",
    "PG_ROTATE_LOGFILE",
    "PG_PROMOTE",
    "PG_SWITCH_WAL",
    "PG_FILE_WRITE",
    "PG_START_BACKUP",
    "PG_STOP_BACKUP",
    "PG_DROP_REPLICATION_SLOT",
    "PG_REPLICATION_SLOT_ADVANCE",
    "PG_LOGICAL_SLOT_GET_CHANGES",
    "PG_LOGICAL_SLOT_GET_BINARY_CHANGES",
    "PG_LOGICAL_EMIT_MESSAGE",
    "PG_LOG_BACKEND_MEMORY_CONTEXTS",
    "PG_IMPORT_SYSTEM_COLLATIONS",
    "PG_STAT_STATEMENTS_RESET",
    "LO_IMPORT",
    "LO_EXPORT",
    "LO_UNLINK",
    "LO_CREAT",
    "LO_CREATE",
    "LO_PUT",
    "LO_FROM_BYTEA",
    "LO_TRUNCATE",
    "LO_TRUNCATE64",
    "LOWRITE",
    "LOAD_EXTENSION",
];

/// Prefixes of function families with side effects (F2): replication slots and origins,
/// statistics resets, backups, WAL replay control, `dblink*`, `pg_create_*` (restore points,
/// replication slots) and `pg_copy_*_replication_slot`. A few read-only members
/// (`pg_replication_origin_oid`…) are over-classified, which only asks for confirmation.
const SIDE_EFFECT_PREFIXES: &[&str] = &[
    "DBLINK",
    "PG_CREATE_",
    "PG_COPY_",
    "PG_STAT_RESET",
    "PG_BACKUP_",
    "PG_WAL_REPLAY_",
    "PG_REPLICATION_ORIGIN_",
];

fn is_side_effect_function(upper: &str) -> bool {
    SIDE_EFFECT_FUNCTIONS.contains(&upper)
        || SIDE_EFFECT_PREFIXES.iter().any(|p| upper.starts_with(p))
}

/// Settings `SET` / `RESET` may change without affecting access mode, privileges or the lexical
/// rules. Everything else is [`StatementClass::ChangesAccessMode`].
const HARMLESS_SETTINGS: &[&str] = &[
    "search_path",
    "schema",
    "timezone",
    "time_zone",
    "time",
    "datestyle",
    "intervalstyle",
    "client_encoding",
    "names",
    "character",
    "character_set_client",
    "character_set_results",
    "character_set_connection",
    "collation_connection",
    "application_name",
    "statement_timeout",
    "lock_timeout",
    "idle_in_transaction_session_timeout",
    "extra_float_digits",
    "bytea_output",
    "work_mem",
    "client_min_messages",
    "lc_messages",
    "lc_monetary",
    "lc_numeric",
    "lc_time",
    "max_execution_time",
    "sql_select_limit",
    "net_read_timeout",
    "net_write_timeout",
    "wait_timeout",
    "lock_wait_timeout",
    "innodb_lock_wait_timeout",
    "div_precision_increment",
    "group_concat_max_len",
];

/// SQLite pragmas that only read schema information.
const READ_PRAGMAS: &[&str] = &[
    "table_info",
    "table_xinfo",
    "table_list",
    "index_list",
    "index_info",
    "index_xinfo",
    "foreign_key_list",
    "database_list",
    "collation_list",
    "compile_options",
    "function_list",
    "module_list",
    "pragma_list",
];

fn word(l: &Located) -> Option<&str> {
    match &l.item {
        Item::Word(w) => Some(w),
        _ => None,
    }
}

/// Lower-cased name of a word or quoted identifier.
fn name(l: &Located) -> Option<String> {
    match &l.item {
        Item::Word(w) => Some(w.to_ascii_lowercase()),
        Item::Ident(i) => Some(i.clone()),
        _ => None,
    }
}

fn classify_items(items: &[Located]) -> StatementClass {
    // Statements must start with a keyword, optionally after opening parentheses.
    let Some(first_idx) = items.iter().position(|l| l.item != Item::Punct(b'(')) else {
        return StatementClass::Unknown;
    };
    let Some(first) = word(&items[first_idx]) else {
        return StatementClass::Unknown;
    };
    let base = classify_by_keyword(first, &items[first_idx..]);

    // Function calls anywhere in the statement.
    let mut worst = base;
    for pair in items.windows(2) {
        if pair[1].item != Item::Punct(b'(') {
            continue;
        }
        let Some(f) = word(&pair[0])
            .map(str::to_owned)
            .or_else(|| name(&pair[0]).map(|n| n.to_ascii_uppercase()))
        else {
            continue;
        };
        if ACCESS_MODE_FUNCTIONS.contains(&f.as_str()) {
            worst = worst.max(StatementClass::ChangesAccessMode);
        } else if is_side_effect_function(&f) {
            worst = worst.max(StatementClass::Unknown);
        }
    }
    worst
}

fn has_word(items: &[Located], w: &str) -> bool {
    items.iter().any(|l| word(l) == Some(w))
}

fn has_top_level_word(items: &[Located], w: &str) -> bool {
    items.iter().any(|l| l.depth == 0 && word(l) == Some(w))
}

fn classify_by_keyword(first: &str, items: &[Located]) -> StatementClass {
    match first {
        "SELECT" | "VALUES" | "TABLE" => {
            if has_word(items, "INTO") {
                StatementClass::Write
            } else {
                StatementClass::Read
            }
        }
        "SHOW" | "DESCRIBE" | "DESC" => StatementClass::Read,
        "EXPLAIN" => {
            let analyze = items
                .iter()
                .position(|l| matches!(word(l), Some("ANALYZE" | "ANALYSE")));
            match analyze {
                // EXPLAIN ANALYZE runs the statement: classify what it runs.
                Some(p) if p + 1 < items.len() => {
                    classify_items(&items[p + 1..]).max(StatementClass::Read)
                }
                Some(_) => StatementClass::Unknown,
                None => StatementClass::Read,
            }
        }
        "WITH" => {
            if ["INSERT", "UPDATE", "DELETE", "MERGE"]
                .iter()
                .any(|w| has_word(items, w))
            {
                StatementClass::Unknown
            } else if has_word(items, "INTO") {
                StatementClass::Write
            } else {
                StatementClass::Read
            }
        }
        "INSERT" | "MERGE" | "UPSERT" | "REPLACE" => StatementClass::Write,
        "UPDATE" | "DELETE" => {
            // Only a top-level WHERE filters the statement; a WHERE inside a subquery does not.
            if has_top_level_word(items, "WHERE") {
                StatementClass::Write
            } else {
                StatementClass::Destructive
            }
        }
        "CREATE" => StatementClass::Ddl,
        "DROP" | "TRUNCATE" | "ALTER" | "GRANT" | "REVOKE" | "RENAME" => {
            StatementClass::Destructive
        }
        "BEGIN" | "START" | "COMMIT" | "ROLLBACK" | "SAVEPOINT" | "RELEASE" | "END" => {
            if first == "START" && !has_word(items, "TRANSACTION") {
                return StatementClass::Unknown;
            }
            if has_word(items, "WRITE") {
                // `BEGIN READ WRITE` / `START TRANSACTION READ WRITE`.
                StatementClass::ChangesAccessMode
            } else {
                StatementClass::Transaction
            }
        }
        "SET" | "RESET" => classify_set(first, &items[1..]),
        "PRAGMA" => classify_pragma(&items[1..]),
        "USE" => StatementClass::Session,
        _ => StatementClass::Unknown,
    }
}

/// `SET` / `RESET`: every assignment must target an allowlisted setting.
fn classify_set(first: &str, rest: &[Located]) -> StatementClass {
    if rest.is_empty() {
        return StatementClass::Unknown;
    }
    let segments = rest.split(|l| l.depth == 0 && l.item == Item::Punct(b','));
    let mut class = StatementClass::Session;
    for (n, seg) in segments.enumerate() {
        // After the first, a segment without `=` / `TO` continues the previous value list
        // (`SET search_path TO a, b`); it names no setting.
        let assigns = seg.iter().any(|l| l.item == Item::Punct(b'=')) || has_word(seg, "TO");
        if n > 0 && !assigns {
            continue;
        }
        let c = classify_set_target(first, seg);
        class = class.max(c);
    }
    class
}

fn classify_set_target(first: &str, seg: &[Located]) -> StatementClass {
    let mut i = 0;
    // `@user_var` (MySQL) is harmless; `@@[session.|local.]name` is a system variable.
    if seg.first().map(|l| &l.item) == Some(&Item::Punct(b'@')) {
        if seg.get(1).map(|l| &l.item) != Some(&Item::Punct(b'@')) {
            return StatementClass::Session;
        }
        i = 2;
        if let (Some(scope), Some(dot)) = (seg.get(i), seg.get(i + 1))
            && dot.item == Item::Punct(b'.')
        {
            match word(scope) {
                Some("SESSION" | "LOCAL") => i += 2,
                _ => return StatementClass::ChangesAccessMode,
            }
        }
    } else {
        // Scope modifiers.
        while let Some(w) = seg.get(i).and_then(word) {
            match w {
                "SESSION" | "LOCAL" => i += 1,
                _ => break,
            }
        }
    }
    let Some(target) = seg.get(i).and_then(name) else {
        return StatementClass::Unknown;
    };
    if first == "RESET" && target == "all" {
        return StatementClass::ChangesAccessMode;
    }
    if target == "transaction" {
        // `SET TRANSACTION ISOLATION LEVEL …` is harmless; `… READ WRITE` is not.
        return if has_word(seg, "WRITE") {
            StatementClass::ChangesAccessMode
        } else {
            StatementClass::Session
        };
    }
    if ENCODING_SETTINGS.contains(&target.as_str()) {
        // The client encoding is a lexical mode (multi-byte encodings such as SJIS or GBK can
        // hide a quote behind a backslash): only UTF-8 values are harmless.
        return if encoding_value(&seg[i + 1..]).is_some_and(|v| is_utf8_value(&target, &v)) {
            StatementClass::Session
        } else {
            StatementClass::ChangesAccessMode
        };
    }
    if HARMLESS_SETTINGS.contains(&target.as_str()) {
        StatementClass::Session
    } else {
        StatementClass::ChangesAccessMode
    }
}

/// Settings that change the client encoding / collation (allowed with UTF-8 values only).
const ENCODING_SETTINGS: &[&str] = &[
    "client_encoding",
    "names",
    "character",
    "character_set_client",
    "character_set_results",
    "character_set_connection",
    "collation_connection",
];

/// First value after the setting name, skipping `=`, `:=`, `TO` and the `SET` of
/// `CHARACTER SET`.
fn encoding_value(rest: &[Located]) -> Option<String> {
    rest.iter()
        .find(|l| {
            !matches!(l.item, Item::Punct(b'=' | b':')) && !matches!(word(l), Some("TO" | "SET"))
        })
        .and_then(|l| match &l.item {
            Item::Word(w) => Some(w.to_ascii_lowercase()),
            Item::Ident(v) | Item::Str(v) => Some(v.clone()),
            Item::Punct(_) => None,
        })
}

fn is_utf8_value(target: &str, value: &str) -> bool {
    let v: String = value.chars().filter(|c| *c != '-').collect();
    if target == "collation_connection" {
        v.starts_with("utf8mb4_") || v.starts_with("utf8_")
    } else {
        matches!(v.as_str(), "utf8" | "utf8mb4")
    }
}

/// `PRAGMA [schema.]name[(arg)| = value]`: only schema-reading pragmas are reads.
fn classify_pragma(rest: &[Located]) -> StatementClass {
    let target = rest
        .iter()
        .take_while(|l| !matches!(l.item, Item::Punct(b'(' | b'=')))
        .filter_map(name)
        .last();
    match target {
        Some(t) if READ_PRAGMAS.contains(&t.as_str()) => StatementClass::Read,
        _ => StatementClass::ChangesAccessMode,
    }
}

// ---------------------------------------------------------------------------------------------
// Literals

/// Civil date from days since 1970-01-01 (proleptic Gregorian).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    // Ranges are guaranteed by the algorithm: 1..=31 and 1..=12.
    (
        y,
        u32::try_from(m).unwrap_or(1),
        u32::try_from(d).unwrap_or(1),
    )
}

/// `^[+-]?\d+(\.\d+)?([eE][+-]?\d+)?$`
fn is_strict_decimal(s: &str) -> bool {
    fn digits(b: &[u8]) -> usize {
        b.iter().take_while(|c| c.is_ascii_digit()).count()
    }
    let b = s.as_bytes();
    let mut i = usize::from(matches!(b.first(), Some(b'+' | b'-')));
    let n = digits(&b[i..]);
    if n == 0 {
        return false;
    }
    i += n;
    if b.get(i) == Some(&b'.') {
        let n = digits(&b[i + 1..]);
        if n == 0 {
            return false;
        }
        i += 1 + n;
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(b.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        let n = digits(&b[i..]);
        if n == 0 {
            return false;
        }
        i += n;
    }
    i == b.len()
}

fn quote_text(s: &str, backslash: bool) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        match c {
            '\'' => out.push_str("''"),
            '\\' if backslash => out.push_str("\\\\"),
            _ => out.push(c),
        }
    }
    out.push('\'');
    out
}

fn ansi_literal(value: &Param, backslash: bool) -> String {
    match value {
        Param::Null => "NULL".to_owned(),
        Param::Bool(b) => if *b { "TRUE" } else { "FALSE" }.to_owned(),
        Param::Int(i) => i.to_string(),
        Param::Float(f) if f.is_finite() => format!("{f:?}"),
        Param::Float(f) => quote_text(&f.to_string(), backslash),
        Param::Decimal(d) => {
            if is_strict_decimal(d) {
                d.clone()
            } else {
                quote_text(d, backslash)
            }
        }
        Param::Text(s) | Param::Json(s) => quote_text(s, backslash),
        Param::Bytes(b) => {
            let mut s = String::with_capacity(b.len() * 2 + 3);
            s.push_str("X'");
            for byte in b {
                s.push_str(&format!("{byte:02X}"));
            }
            s.push('\'');
            s
        }
        Param::Date(days) => {
            let (y, m, d) = civil_from_days(i64::from(*days));
            format!("DATE '{y:04}-{m:02}-{d:02}'")
        }
        Param::Timestamp { micros, utc } => {
            let secs = micros.div_euclid(1_000_000);
            let frac = micros.rem_euclid(1_000_000);
            let days = secs.div_euclid(86_400);
            let sod = secs.rem_euclid(86_400);
            let (y, m, d) = civil_from_days(days);
            let (hh, mm, ss) = (sod / 3600, (sod % 3600) / 60, sod % 60);
            let tz = if *utc { "+00:00" } else { "" };
            format!("TIMESTAMP '{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}:{ss:02}.{frac:06}{tz}'")
        }
    }
}

// ---------------------------------------------------------------------------------------------
// DML generation

fn render_edit_ansi<D: Dialect + ?Sized>(d: &D, edit: &RowEdit) -> DriverResult<Statement> {
    if edit.table.is_empty() {
        return Err(DriverError::InvalidParams("edit without a table".into()));
    }
    let table = d.qualified_name(&edit.table);
    let mut params = Vec::new();
    let mut next = |p: &Param, params: &mut Vec<Param>| {
        params.push(p.clone());
        d.placeholder(params.len())
    };
    let text = match &edit.change {
        RowChange::Insert { values } => {
            if values.is_empty() {
                format!("INSERT INTO {table} DEFAULT VALUES")
            } else {
                let cols: Vec<String> = values.iter().map(|(c, _)| d.quote_identifier(c)).collect();
                let phs: Vec<String> = values.iter().map(|(_, v)| next(v, &mut params)).collect();
                format!(
                    "INSERT INTO {table} ({}) VALUES ({})",
                    cols.join(", "),
                    phs.join(", ")
                )
            }
        }
        RowChange::Update { key, values } => {
            if key.is_empty() {
                return Err(DriverError::InvalidParams(
                    "update without a unique key".into(),
                ));
            }
            if values.is_empty() {
                return Err(DriverError::InvalidParams("update without values".into()));
            }
            let sets: Vec<String> = values
                .iter()
                .map(|(c, v)| format!("{} = {}", d.quote_identifier(c), next(v, &mut params)))
                .collect();
            let wh = where_clause(d, key, &mut params, &mut next);
            format!("UPDATE {table} SET {} WHERE {wh}", sets.join(", "))
        }
        RowChange::Delete { key } => {
            if key.is_empty() {
                return Err(DriverError::InvalidParams(
                    "delete without a unique key".into(),
                ));
            }
            let wh = where_clause(d, key, &mut params, &mut next);
            format!("DELETE FROM {table} WHERE {wh}")
        }
    };
    Ok(Statement::sql(text).with_params(params))
}

fn where_clause<D: Dialect + ?Sized>(
    d: &D,
    key: &[(String, Param)],
    params: &mut Vec<Param>,
    next: &mut impl FnMut(&Param, &mut Vec<Param>) -> String,
) -> String {
    key.iter()
        .map(|(c, v)| {
            let col = d.quote_identifier(c);
            if *v == Param::Null {
                format!("{col} IS NULL")
            } else {
                format!("{col} = {}", next(v, params))
            }
        })
        .collect::<Vec<_>>()
        .join(" AND ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn texts(script: &str) -> Vec<&str> {
        AnsiDialect
            .split_statements(script)
            .iter()
            .map(|s| s.text(script))
            .collect()
    }

    #[test]
    fn splits_on_top_level_semicolons() {
        let s = "select 1; select ';' ; -- c; x\n select \"a;b\" /* ; */ from t;;  ";
        assert_eq!(
            texts(s),
            vec!["select 1", "select ';'", "select \"a;b\" /* ; */ from t"]
        );
    }

    #[test]
    fn comment_only_script_is_empty() {
        assert!(texts("-- nothing\n/* here */ ;").is_empty());
    }

    #[test]
    fn dialect_rules() {
        let pg = LexicalRules::POSTGRES;
        let s = "create function f() as $body$ select 1; $body$; /* a /* b; */ c; */ select $1;";
        let spans = split_with_rules(s, pg);
        assert_eq!(spans.len(), 2, "{spans:?}");
        assert_eq!(spans[1].text(s), "select $1");

        let my = LexicalRules::MYSQL;
        let s = "select 'it\\'s; ok', `a;b` # x; y\n; select 2";
        let spans = split_with_rules(s, my);
        assert_eq!(spans.len(), 2, "{spans:?}");
    }

    #[test]
    fn classifies() {
        use StatementClass as C;
        let d = AnsiDialect;
        for (sql, want) in [
            ("SELECT * FROM t", C::Read),
            ("  (select 1)", C::Read),
            ("-- x\nshow tables", C::Read),
            ("select * into t2 from t", C::Write),
            ("insert into t values (1)", C::Write),
            ("update t set a = 1 where id = 2", C::Write),
            ("update t set a = 1", C::Destructive),
            ("DELETE FROM t", C::Destructive),
            ("delete from t /* where */", C::Destructive),
            ("delete from t where 'x' = 'where'", C::Write),
            ("drop table t", C::Destructive),
            ("truncate t", C::Destructive),
            ("alter table t add c int", C::Destructive),
            ("grant select on t to u", C::Destructive),
            ("create table t (a int)", C::Ddl),
            ("begin", C::Transaction),
            ("commit", C::Transaction),
            ("set search_path = app", C::Session),
            (
                "SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE",
                C::ChangesAccessMode,
            ),
            (
                "set default_transaction_read_only = off",
                C::ChangesAccessMode,
            ),
            ("start transaction read write", C::ChangesAccessMode),
            ("reset all", C::ChangesAccessMode),
            (
                "with x as (delete from t returning *) select * from x",
                C::Unknown,
            ),
            ("with x as (select 1) select * from x", C::Read),
            ("explain select 1", C::Read),
            ("explain analyze delete from t", C::Destructive),
            ("vacuum", C::Unknown),
            ("call p()", C::Unknown),
            ("'oops'", C::Unknown),
            ("select 1; drop table t", C::Destructive),
            ("", C::Read),
        ] {
            assert_eq!(d.classify(sql), want, "{sql}");
        }
        assert!(C::Unknown.needs_confirmation());
        assert!(C::Unknown.is_write());
        assert!(!C::Read.is_write());
    }

    /// Bypasses found in the P0-B security review (H1, H2, M5). Each must stay caught.
    #[test]
    fn security_review_corpus() {
        use StatementClass as C;
        let my = LexicalRules::MYSQL;
        let pg = LexicalRules::POSTGRES;
        let ansi = LexicalRules::ANSI;
        for (rules, sql, at_least, spans) in [
            // H1: MySQL / MariaDB executable comments are code.
            (my, "SELECT 1 /*! ; DROP TABLE users */", C::Destructive, 2),
            (my, "/*!50000 DROP TABLE users */", C::Destructive, 1),
            (my, "/*M! DROP TABLE users */", C::Destructive, 1),
            (my, "/*M!100100 DROP TABLE users */", C::Destructive, 1),
            // ...and count as Unknown when the rules do not execute them.
            (ansi, "/*!50000 DROP TABLE users */", C::Unknown, 0),
            (pg, "SELECT 1 /*! DROP TABLE users */", C::Unknown, 1),
            // H1: `--` needs a following space in MySQL.
            (my, "SELECT 1--1; DROP TABLE users", C::Destructive, 2),
            (my, "SELECT 1-- comment; DROP TABLE users", C::Read, 1),
            // H1: PostgreSQL E-strings take backslash escapes.
            (
                pg,
                "SELECT E'\\''; DROP TABLE users; --'",
                C::Destructive,
                2,
            ),
            (
                pg,
                "SELECT e'\\''; DROP TABLE users; --'",
                C::Destructive,
                2,
            ),
            // H1: MySQL default mode vs NO_BACKSLASH_ESCAPES.
            (my, "SELECT 'a\\'; DROP TABLE users; -- '", C::Read, 1),
            (
                my.with_backslash_escapes(false),
                "SELECT 'a\\'; DROP TABLE users; -- '",
                C::Destructive,
                2,
            ),
            // H2: quoted setting names and functions that lift read-only.
            (
                pg,
                "SET \"default_transaction_read_only\" = off",
                C::ChangesAccessMode,
                1,
            ),
            (
                pg,
                "SET default_transaction_read_only = off",
                C::ChangesAccessMode,
                1,
            ),
            (
                pg,
                "SET SESSION default_transaction_read_only TO off",
                C::ChangesAccessMode,
                1,
            ),
            (
                pg,
                "SET search_path = app, transaction_read_only = off",
                C::ChangesAccessMode,
                1,
            ),
            (
                pg,
                "SELECT set_config('default_transaction_read_only','off',false)",
                C::ChangesAccessMode,
                1,
            ),
            (
                pg,
                "select * from (select SET_CONFIG('a','b',false)) x",
                C::ChangesAccessMode,
                1,
            ),
            (my, "SET `transaction_read_only`=0", C::ChangesAccessMode, 1),
            (
                my,
                "SET @@session.transaction_read_only = 0",
                C::ChangesAccessMode,
                1,
            ),
            (
                my,
                "SET @@global.time_zone = '+00:00'",
                C::ChangesAccessMode,
                1,
            ),
            (
                my,
                "SET GLOBAL time_zone = '+00:00'",
                C::ChangesAccessMode,
                1,
            ),
            (
                my,
                "SET sql_mode = 'NO_BACKSLASH_ESCAPES'",
                C::ChangesAccessMode,
                1,
            ),
            (ansi, "SET `transaction_read_only`=0", C::Unknown, 1),
            (pg, "SET ROLE admin", C::ChangesAccessMode, 1),
            (
                pg,
                "RESET default_transaction_read_only",
                C::ChangesAccessMode,
                1,
            ),
            (ansi, "PRAGMA query_only = OFF", C::ChangesAccessMode, 1),
            (ansi, "PRAGMA query_only(0)", C::ChangesAccessMode, 1),
            (ansi, "PRAGMA main.table_info(users)", C::Read, 1),
            (pg, "select nextval('s')", C::Unknown, 1),
            (pg, "select pg_terminate_backend(42)", C::Unknown, 1),
            // N1: `$` inside PostgreSQL identifiers does not open a dollar quote.
            (
                pg,
                "WITH a$q$ AS (DELETE FROM users RETURNING 1) SELECT 1 AS b$q$",
                C::Unknown,
                1,
            ),
            (
                pg,
                "SELECT x$q$, set_config('default_transaction_read_only','off',false) AS y$q$",
                C::ChangesAccessMode,
                1,
            ),
            (pg, "SELECT $q$ ; $q$ AS a$b", C::Read, 1),
            (pg, "SELECT $1, $q$x$q$", C::Read, 1),
            // N4: encoding changes are lexical-mode changes unless UTF-8.
            (my, "SET NAMES sjis", C::ChangesAccessMode, 1),
            (my, "SET NAMES 'gbk'", C::ChangesAccessMode, 1),
            (my, "SET CHARACTER SET big5", C::ChangesAccessMode, 1),
            (
                my,
                "SET character_set_client = gbk",
                C::ChangesAccessMode,
                1,
            ),
            (
                my,
                "SET collation_connection = gbk_chinese_ci",
                C::ChangesAccessMode,
                1,
            ),
            (pg, "SET client_encoding TO 'SJIS'", C::ChangesAccessMode, 1),
            (pg, "RESET client_encoding", C::ChangesAccessMode, 1),
            (pg, "SET client_encoding TO 'UTF-8'", C::Session, 1),
            (pg, "SET client_encoding = UTF8", C::Session, 1),
            (my, "SET CHARACTER SET utf8mb4", C::Session, 1),
            (
                my,
                "SET collation_connection = utf8mb4_general_ci",
                C::Session,
                1,
            ),
            // Harmless settings stay Session.
            (pg, "SET search_path TO app, public", C::Session, 1),
            (pg, "SET TIME ZONE 'UTC'", C::Session, 1),
            (my, "SET NAMES utf8mb4", C::Session, 1),
            (my, "SET @x = 1", C::Session, 1),
            (my, "SET @@session.time_zone = '+00:00'", C::Session, 1),
            (
                pg,
                "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE",
                C::Session,
                1,
            ),
            (pg, "SET TRANSACTION READ WRITE", C::ChangesAccessMode, 1),
            // F1: Unicode-escaped names are decoded before matching.
            (
                pg,
                "SELECT U&\"set\\005fconfig\"('default_transaction_read_only','off',false)",
                C::ChangesAccessMode,
                1,
            ),
            (
                pg,
                "SELECT u&\"\\0070g_terminate_backend\"(42)",
                C::Unknown,
                1,
            ),
            (
                pg,
                "SET U&\"default\\005ftransaction\\005fread\\005fonly\" = off",
                C::ChangesAccessMode,
                1,
            ),
            (pg, "SET U&\"search\\005fpath\" = app", C::Session, 1),
            (
                pg,
                "SELECT U&\"!0073et_config\" UESCAPE '!'('a','b',false)",
                C::Unknown,
                1,
            ),
            (pg, "SELECT U&\"bad\\zzzz\"(1)", C::Unknown, 1),
            (pg, "SELECT U&\"\\D83D\\DE00\" FROM t", C::Read, 1),
            (pg, "SELECT U&\"\\D83D\" FROM t", C::Unknown, 1),
            (pg, "SELECT U&'a;b'; DROP TABLE t", C::Destructive, 2),
            (
                pg,
                "SET client_encoding TO U&'\\0053JIS'",
                C::ChangesAccessMode,
                1,
            ),
            (pg, "SET client_encoding TO U&'UTF8'", C::Session, 1),
            // F2: more side-effect functions.
            (
                pg,
                "SELECT pg_create_logical_replication_slot('s', 'pgoutput')",
                C::Unknown,
                1,
            ),
            (
                pg,
                "SELECT * FROM pg_create_physical_replication_slot('s')",
                C::Unknown,
                1,
            ),
            (pg, "SELECT pg_drop_replication_slot('s')", C::Unknown, 1),
            (pg, "SELECT pg_stat_reset()", C::Unknown, 1),
            (pg, "SELECT pg_stat_reset_shared('bgwriter')", C::Unknown, 1),
            (pg, "SELECT pg_backup_start('x')", C::Unknown, 1),
            (pg, "SELECT pg_backup_stop()", C::Unknown, 1),
            (pg, "SELECT pg_wal_replay_pause()", C::Unknown, 1),
            (pg, "SELECT pg_wal_replay_resume()", C::Unknown, 1),
            (
                pg,
                "SELECT pg_logical_emit_message(true, 'p', 'x')",
                C::Unknown,
                1,
            ),
            (
                pg,
                "SELECT pg_replication_origin_create('o')",
                C::Unknown,
                1,
            ),
            (
                pg,
                "SELECT pg_replication_origin_session_setup('o')",
                C::Unknown,
                1,
            ),
            (pg, "SELECT dblink_open('c', 'select 1')", C::Unknown, 1),
            (
                pg,
                "SELECT pg_copy_logical_replication_slot('a', 'b')",
                C::Unknown,
                1,
            ),
            (
                pg,
                "SELECT pg_replication_slot_advance('s', '0/0')",
                C::Unknown,
                1,
            ),
            (
                pg,
                "SELECT pg_logical_slot_get_changes('s', NULL, NULL)",
                C::Unknown,
                1,
            ),
            // The limit is documented: unknown user functions stay Read (L4).
            (pg, "SELECT my_app_cleanup()", C::Read, 1),
            (
                pg,
                "SELECT pg_logical_slot_peek_changes('s', NULL, NULL)",
                C::Read,
                1,
            ),
            // M5: WHERE must be at the top level.
            (
                pg,
                "UPDATE users SET active = (SELECT false FROM flags WHERE id = 1)",
                C::Destructive,
                1,
            ),
            (
                pg,
                "DELETE FROM t WHERE id IN (SELECT id FROM u)",
                C::Write,
                1,
            ),
            (
                pg,
                "DELETE FROM t USING (SELECT 1 WHERE true) x",
                C::Destructive,
                1,
            ),
        ] {
            let got = classify_with_rules(sql, rules);
            assert!(
                got >= at_least,
                "{sql}: got {got:?}, want at least {at_least:?}"
            );
            if at_least <= C::Session {
                assert_eq!(got, at_least, "{sql}");
            }
            assert_eq!(split_with_rules(sql, rules).len(), spans, "{sql}");
        }
    }

    #[test]
    fn single_statement_guard() {
        struct My;
        impl Dialect for My {
            fn lexical_rules(&self) -> LexicalRules {
                LexicalRules::MYSQL
            }
        }
        assert!(ensure_single_statement(&AnsiDialect, "select 1;").is_ok());
        assert!(ensure_single_statement(&AnsiDialect, "-- only a comment").is_ok());
        assert_eq!(
            ensure_single_statement(&AnsiDialect, "select 1; select 2"),
            Err(DriverError::MultipleStatements(2))
        );
        assert_eq!(
            ensure_single_statement(&My, "SELECT 1 /*! ; DROP TABLE users */"),
            Err(DriverError::MultipleStatements(2))
        );
    }

    #[test]
    fn strict_decimals() {
        for ok in ["0", "-12.50", "+1", "1e10", "1.5E-3"] {
            assert!(is_strict_decimal(ok), "{ok}");
        }
        for bad in [
            "", "-", "1.", ".5", "1e", "1-2", "--1", "1e+", "1.2.3", "1 ", "0x10",
        ] {
            assert!(!is_strict_decimal(bad), "{bad}");
        }
    }

    #[test]
    fn quoting_and_literals() {
        let d = AnsiDialect;
        assert_eq!(d.quote_identifier("a\"b"), "\"a\"\"b\"");
        assert_eq!(
            d.qualified_name(&["public".into(), "users".into()]),
            "\"public\".\"users\""
        );
        assert_eq!(d.format_literal(&Param::Text("it's".into())), "'it''s'");
        assert_eq!(d.format_literal(&Param::Null), "NULL");
        assert_eq!(d.format_literal(&Param::Bytes(vec![0xde, 0xad])), "X'DEAD'");
        assert_eq!(d.format_literal(&Param::Date(0)), "DATE '1970-01-01'");
        assert_eq!(d.format_literal(&Param::Date(19_723)), "DATE '2024-01-01'");
        assert_eq!(d.format_literal(&Param::Date(-1)), "DATE '1969-12-31'");
        assert_eq!(
            d.format_literal(&Param::Timestamp {
                micros: 1_500_000,
                utc: true
            }),
            "TIMESTAMP '1970-01-01 00:00:01.500000+00:00'"
        );
        assert_eq!(d.format_literal(&Param::Decimal("-12.50".into())), "-12.50");
        assert_eq!(
            d.format_literal(&Param::Decimal("1; drop table t".into())),
            "'1; drop table t'"
        );
        assert_eq!(d.format_literal(&Param::Float(1.0)), "1.0");
        assert_eq!(
            d.page_query("select 1;", 10, 20).as_deref(),
            Some("SELECT * FROM (select 1) AS ninesql_page LIMIT 10 OFFSET 20")
        );
        assert_eq!(d.explain("select 1", ExplainFormat::Text), None);
    }

    #[test]
    fn renders_edits_with_params() {
        let d = AnsiDialect;
        let s = d
            .render_edit(&RowEdit {
                table: vec!["app".into(), "users".into()],
                change: RowChange::Update {
                    key: vec![("id".into(), Param::Int(7)), ("tenant".into(), Param::Null)],
                    values: vec![("name".into(), Param::Text("x".into()))],
                },
            })
            .unwrap();
        assert_eq!(
            s.text,
            "UPDATE \"app\".\"users\" SET \"name\" = ? WHERE \"id\" = ? AND \"tenant\" IS NULL"
        );
        assert_eq!(s.params, vec![Param::Text("x".into()), Param::Int(7)]);

        let s = d
            .render_edit(&RowEdit {
                table: vec!["t".into()],
                change: RowChange::Insert {
                    values: vec![("a".into(), Param::Int(1)), ("b".into(), Param::Bool(true))],
                },
            })
            .unwrap();
        assert_eq!(s.text, "INSERT INTO \"t\" (\"a\", \"b\") VALUES (?, ?)");

        let s = d
            .render_edit(&RowEdit {
                table: vec!["t".into()],
                change: RowChange::Delete {
                    key: vec![("id".into(), Param::Int(1))],
                },
            })
            .unwrap();
        assert_eq!(s.text, "DELETE FROM \"t\" WHERE \"id\" = ?");

        let err = d.render_edit(&RowEdit {
            table: vec!["t".into()],
            change: RowChange::Delete { key: vec![] },
        });
        assert!(matches!(err, Err(DriverError::InvalidParams(_))));
    }

    fn any_rules() -> impl Strategy<Value = LexicalRules> {
        prop::collection::vec(any::<bool>(), 8).prop_map(|b| {
            LexicalRules::ANSI
                .with_backslash_escapes(b[0])
                .with_e_string_backslash(b[1])
                .with_nested_block_comments(b[2])
                .with_dollar_quotes(b[3])
                .with_hash_comments(b[4])
                .with_dash_comment_needs_space(b[5])
                .with_executable_comments(b[6])
                .with_backtick_identifiers(b[7])
        })
    }

    #[test]
    fn unicode_escapes_decode() {
        assert_eq!(
            decode_unicode_escaped("U&\"d\\0061t\\+000061\"").as_deref(),
            Some("data")
        );
        assert_eq!(decode_unicode_escaped("u&'it''s'").as_deref(), Some("it's"));
        assert_eq!(
            decode_unicode_escaped("U&\"a\\\\b\"").as_deref(),
            Some("a\\b")
        );
        assert_eq!(
            decode_unicode_escaped("U&\"\\D83D\\DE00\"").as_deref(),
            Some("\u{1F600}")
        );
        for bad in [
            "U&\"\\12\"",
            "U&\"\\+12\"",
            "U&\"\\DE00\"",
            "U&\"\\D83Dx\"",
            "U&\"\\+110000\"",
            "U&\"abc",
            "U&\"",
        ] {
            assert_eq!(decode_unicode_escaped(bad), None, "{bad}");
        }
    }

    proptest! {
        /// Any name, Unicode-escaped character by character, decodes back to itself.
        #[test]
        fn unicode_escaped_names_roundtrip(name in "\\PC{1,20}") {
            let mut quoted = String::from("U&\"");
            for c in name.chars() {
                quoted.push_str(&format!("\\+{:06X}", u32::from(c)));
            }
            quoted.push('"');
            prop_assert_eq!(decode_unicode_escaped(&quoted), Some(name));
        }

        #[test]
        fn spans_are_ordered_and_on_char_boundaries(script in "\\PC{0,300}", rules in any_rules()) {
            let spans = split_with_rules(&script, rules);
            let mut last = 0usize;
            for s in &spans {
                prop_assert!(s.start < s.end);
                prop_assert!(s.start >= last);
                prop_assert!(s.end <= script.len());
                prop_assert!(script.is_char_boundary(s.start));
                prop_assert!(script.is_char_boundary(s.end));
                prop_assert!(!s.text(&script).trim().is_empty());
                last = s.end;
            }
            let _ = classify_with_rules(&script, rules);
        }

        #[test]
        fn simple_statements_roundtrip(
            stmts in prop::collection::vec("select [a-z]{1,8} from [a-z]{1,8}( where [a-z] = '[a-z; ]{0,6}')?", 1..8),
        ) {
            let script = stmts.join(";\n");
            let got: Vec<String> = split_with_rules(&script, LexicalRules::default())
                .iter()
                .map(|s| s.text(&script).to_owned())
                .collect();
            prop_assert_eq!(got, stmts);
        }

        #[test]
        fn quoted_identifiers_roundtrip_as_one_token(name in "\\PC{0,30}") {
            let q = AnsiDialect.quote_identifier(&name);
            let toks = lex(&q, LexicalRules::default()).toks;
            prop_assert_eq!(toks.len(), 1);
            prop_assert_eq!(toks[0].kind, TokKind::Ident);
            prop_assert_eq!(toks[0].end, q.len());
        }

        #[test]
        fn text_literals_are_one_token(value in "\\PC{0,30}", backslash in any::<bool>()) {
            let lit = ansi_literal(&Param::Text(value), backslash);
            let rules = LexicalRules::ANSI.with_backslash_escapes(backslash);
            let toks = lex(&lit, rules).toks;
            prop_assert_eq!(toks.len(), 1);
            prop_assert_eq!(toks[0].end, lit.len());
        }
    }
}
