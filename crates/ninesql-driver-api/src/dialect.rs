//! SQL dialects (ADR-0004): identifier quoting, literals, statement splitting, paging, explain,
//! statement classification for the production guard (ADR-0007) and DML generation for edits.
//!
//! Every method has an ANSI default so a driver only overrides what its engine does differently.
//! The defaults are conservative: anything the classifier does not recognise is
//! [`StatementClass::Unknown`], which counts as destructive.

use crate::capabilities::ExplainFormat;
use crate::error::{DriverError, DriverResult};
use crate::statement::{Param, Statement};

/// Lexical rules used by the default splitter, classifier and literal formatter.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[allow(clippy::struct_excessive_bools)]
pub struct LexicalRules {
    /// Backslash escapes inside string literals (MySQL / MariaDB default mode).
    pub backslash_escapes: bool,
    /// Block comments nest (PostgreSQL).
    pub nested_block_comments: bool,
    /// `$tag$ … $tag$` dollar-quoted strings (PostgreSQL family).
    pub dollar_quotes: bool,
    /// `#` starts a line comment (MySQL family).
    pub hash_comments: bool,
    /// `` ` `` quotes identifiers (MySQL family, SQLite, ClickHouse).
    pub backtick_identifiers: bool,
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
    Quoted,
    Semicolon,
    Other,
}

#[derive(Clone, Copy, Debug)]
struct Tok {
    kind: TokKind,
    start: usize,
    end: usize,
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
    if let Some(&b) = bytes.get(i) {
        if b.is_ascii_digit() {
            return None;
        }
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

fn lex(text: &str, rules: LexicalRules) -> Vec<Tok> {
    let bytes = text.as_bytes();
    let mut toks = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        let next = bytes.get(i + 1).copied();
        if b.is_ascii_whitespace() {
            i += 1;
        } else if b == b'-' && next == Some(b'-') || rules.hash_comments && b == b'#' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
        } else if b == b'/' && next == Some(b'*') {
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
        } else if b == b'\'' || b == b'"' || rules.backtick_identifiers && b == b'`' {
            let backslash = rules.backslash_escapes && b != b'`';
            let end = skip_quoted(bytes, i, b, backslash).min(bytes.len());
            toks.push(Tok {
                kind: TokKind::Quoted,
                start: i,
                end,
            });
            i = end;
        } else if rules.dollar_quotes && b == b'$' {
            if let Some(len) = dollar_tag(bytes, i) {
                let tag = &bytes[i..i + len];
                let end = find_sub(bytes, tag, i + len).map_or(bytes.len(), |p| p + len);
                toks.push(Tok {
                    kind: TokKind::Quoted,
                    start: i,
                    end,
                });
                i = end;
            } else {
                toks.push(Tok {
                    kind: TokKind::Other,
                    start: i,
                    end: i + 1,
                });
                i += 1;
            }
        } else if b == b';' {
            toks.push(Tok {
                kind: TokKind::Semicolon,
                start: i,
                end: i + 1,
            });
            i += 1;
        } else if is_word_byte(b) {
            let start = i;
            while i < bytes.len() && is_word_byte(bytes[i]) {
                i += 1;
            }
            toks.push(Tok {
                kind: TokKind::Word,
                start,
                end: i,
            });
        } else {
            toks.push(Tok {
                kind: TokKind::Other,
                start: i,
                end: i + 1,
            });
            i += 1;
        }
    }
    toks
}

/// Splits `script` with the given lexical rules (the default of [`Dialect::split_statements`]).
#[must_use]
pub fn split_with_rules(script: &str, rules: LexicalRules) -> Vec<StatementSpan> {
    let mut spans = Vec::new();
    let mut current: Option<StatementSpan> = None;
    for t in lex(script, rules) {
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

/// Classifies `text` with the given lexical rules (the default of [`Dialect::classify`]).
#[must_use]
pub fn classify_with_rules(text: &str, rules: LexicalRules) -> StatementClass {
    let toks = lex(text, rules);
    let mut worst: Option<StatementClass> = None;
    for stmt in toks.split(|t| t.kind == TokKind::Semicolon) {
        if stmt.is_empty() {
            continue;
        }
        let words: Vec<String> = stmt
            .iter()
            .filter(|t| t.kind == TokKind::Word)
            .map(|t| text[t.start..t.end].to_ascii_uppercase())
            .collect();
        // Statements must start with a keyword, optionally after opening parentheses.
        let starts_with_word = stmt
            .iter()
            .find(|t| !(t.kind == TokKind::Other && &text[t.start..t.end] == "("))
            .is_some_and(|t| t.kind == TokKind::Word);
        let c = if starts_with_word {
            classify_words(&words)
        } else {
            StatementClass::Unknown
        };
        worst = Some(worst.map_or(c, |w| w.max(c)));
    }
    worst.unwrap_or(StatementClass::Read)
}

const ACCESS_MODE_WORDS: &[&str] = &[
    "READ",
    "WRITE",
    "READONLY",
    "READ_ONLY",
    "QUERY_ONLY",
    "ACCESS_MODE",
    "TRANSACTION",
    "DEFAULT_TRANSACTION_READ_ONLY",
    "TRANSACTION_READ_ONLY",
    "TX_READ_ONLY",
    "SESSION_AUTHORIZATION",
    "ROLE",
];

fn classify_words(words: &[String]) -> StatementClass {
    let Some(first) = words.first() else {
        return StatementClass::Unknown;
    };
    let has = |w: &str| words.iter().any(|x| x == w);
    match first.as_str() {
        "SELECT" | "VALUES" | "TABLE" => {
            if has("INTO") {
                StatementClass::Write
            } else {
                StatementClass::Read
            }
        }
        "SHOW" | "DESCRIBE" | "DESC" => StatementClass::Read,
        "EXPLAIN" => {
            if has("ANALYZE") || has("ANALYSE") {
                // EXPLAIN ANALYZE runs the statement: classify what it runs.
                let rest: Vec<String> = words
                    .iter()
                    .skip_while(|w| !matches!(w.as_str(), "ANALYZE" | "ANALYSE"))
                    .skip(1)
                    .cloned()
                    .collect();
                if rest.is_empty() {
                    StatementClass::Unknown
                } else {
                    classify_words(&rest).max(StatementClass::Read)
                }
            } else {
                StatementClass::Read
            }
        }
        "WITH" => {
            if ["INSERT", "UPDATE", "DELETE", "MERGE"]
                .iter()
                .any(|w| has(w))
            {
                StatementClass::Unknown
            } else if has("INTO") {
                StatementClass::Write
            } else {
                StatementClass::Read
            }
        }
        "INSERT" | "MERGE" | "UPSERT" | "REPLACE" => StatementClass::Write,
        "UPDATE" | "DELETE" => {
            if has("WHERE") {
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
            if first == "START" && !has("TRANSACTION") {
                return StatementClass::Unknown;
            }
            if words.iter().any(|w| w == "WRITE") {
                // `BEGIN READ WRITE` / `START TRANSACTION READ WRITE`.
                StatementClass::ChangesAccessMode
            } else {
                StatementClass::Transaction
            }
        }
        "SET" | "RESET" | "USE" => {
            let touches_access_mode = words
                .iter()
                .skip(1)
                .any(|w| ACCESS_MODE_WORDS.contains(&w.as_str()));
            if touches_access_mode || first == "RESET" && has("ALL") {
                StatementClass::ChangesAccessMode
            } else {
                StatementClass::Session
            }
        }
        _ => StatementClass::Unknown,
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
            let numeric = !d.is_empty()
                && d.chars()
                    .all(|c| c.is_ascii_digit() || matches!(c, '+' | '-' | '.' | 'e' | 'E'))
                && d.chars().any(|c| c.is_ascii_digit());
            if numeric {
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
        let pg = LexicalRules {
            dollar_quotes: true,
            nested_block_comments: true,
            ..LexicalRules::default()
        };
        let s = "create function f() as $body$ select 1; $body$; /* a /* b; */ c; */ select $1;";
        let spans = split_with_rules(s, pg);
        assert_eq!(spans.len(), 2, "{spans:?}");
        assert_eq!(spans[1].text(s), "select $1");

        let my = LexicalRules {
            backslash_escapes: true,
            hash_comments: true,
            backtick_identifiers: true,
            ..LexicalRules::default()
        };
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
        (
            any::<bool>(),
            any::<bool>(),
            any::<bool>(),
            any::<bool>(),
            any::<bool>(),
        )
            .prop_map(|(a, b, c, d, e)| LexicalRules {
                backslash_escapes: a,
                nested_block_comments: b,
                dollar_quotes: c,
                hash_comments: d,
                backtick_identifiers: e,
            })
    }

    proptest! {
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
            let toks = lex(&q, LexicalRules::default());
            prop_assert_eq!(toks.len(), 1);
            prop_assert_eq!(toks[0].kind, TokKind::Quoted);
            prop_assert_eq!(toks[0].end, q.len());
        }

        #[test]
        fn text_literals_are_one_token(value in "\\PC{0,30}", backslash in any::<bool>()) {
            let lit = ansi_literal(&Param::Text(value), backslash);
            let rules = LexicalRules { backslash_escapes: backslash, ..LexicalRules::default() };
            let toks = lex(&lit, rules);
            prop_assert_eq!(toks.len(), 1);
            prop_assert_eq!(toks[0].end, lit.len());
        }
    }
}
