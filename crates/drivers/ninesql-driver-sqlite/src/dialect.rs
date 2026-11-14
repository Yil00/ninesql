//! SQLite dialect: ANSI defaults with SQLite's lexical rules (`` `…` `` and `[…]` identifiers,
//! Tcl-style bind variables, no backslash escapes, non-nested block comments),
//! `EXPLAIN QUERY PLAN` and `?` parameters.
//!
//! The shared lexer of `ninesql-driver-api` knows neither `[…]` identifiers (an SQL Server habit
//! SQLite accepts) nor SQLite's Tcl-style variables (`$a(…)`, `:a(…)`, `@a(…)`, `#a(…)`,
//! `$a::b(…)`, whose parenthesised part swallows everything up to whitespace or `)`, quotes
//! included). Before splitting or classifying, [`lexer_view`] follows SQLite's tokenizer and
//! rewrites both into forms the shared lexer reads as one token, keeping every byte offset, so
//! a quote, a comment marker or a `;` inside them cannot hide a statement from the classifier
//! (security review H1).

use ninesql_driver_api::dialect::{classify_with_rules, split_with_rules};
use ninesql_driver_api::{Dialect, ExplainFormat, LexicalRules, StatementClass, StatementSpan};

/// The SQLite dialect.
#[derive(Clone, Copy, Debug, Default)]
pub struct SqliteDialect;

impl SqliteDialect {
    /// The dialect.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

/// SQLite's `IdChar`: ASCII letters and digits, `_`, `$` and every byte of a multi-byte
/// UTF-8 character.
fn is_id_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b >= 0x80
}

/// SQLite's `sqlite3Isspace`.
fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// The text as the shared lexer must see it to agree with SQLite's tokenizer, byte for byte:
/// - a `[…]` identifier becomes a `"…"` identifier: `[` and the closing `]` become `"`, a `"`
///   inside becomes `_` (the first `]` ends it; an unterminated one runs to the end);
/// - in a variable `$name`, `:name`, `@name` or `#name` (SQLite's `CC_VARALPHA` / `CC_DOLLAR`
///   loop: identifier characters, `::` segments, then an optional `(` that runs to the next
///   whitespace or `)`), the parenthesised part becomes `_` bytes;
/// - identifiers and numbers are consumed whole (`a$b(` is a function call, not a variable).
///
/// Strings (`'…'`, `x'…'`), `"…"` and `` `…` `` identifiers and comments are copied unchanged.
/// Only ASCII bytes are written and only over whole tokens, so the result is valid UTF-8 of the
/// same length, with the same character boundaries at every token edge.
#[must_use]
pub fn lexer_view(text: &str) -> String {
    scan(text, |_| {}).0
}

/// The scanner behind [`lexer_view`]; `on_word` receives every bare identifier or keyword token
/// in code (never text inside strings, quoted or bracketed identifiers, variables or comments).
/// The flag is `true` when the text is incomplete for SQLite: it ends inside an unterminated
/// token (string, quoted or bracketed identifier, block comment, variable parentheses), or its
/// parentheses in code do not balance.
fn scan(text: &str, mut on_word: impl FnMut(&str)) -> (String, bool) {
    let src = text.as_bytes();
    let mut out = src.to_vec();
    let mut unterminated = false;
    let mut depth: i64 = 0;
    let mut i = 0;
    while i < src.len() {
        let c = src[i];
        match c {
            b'\'' | b'"' | b'`' => {
                // Quoted token: a doubled quote stays inside it.
                i += 1;
                while i < src.len() {
                    if src[i] == c {
                        if src.get(i + 1) == Some(&c) {
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
                i += 1;
            }
            b'-' if src.get(i + 1) == Some(&b'-') => {
                while i < src.len() && src[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if src.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i < src.len() && !(src[i] == b'*' && src.get(i + 1) == Some(&b'/')) {
                    i += 1;
                }
                i += 2;
            }
            b'[' => {
                out[i] = b'"';
                i += 1;
                while i < src.len() && src[i] != b']' {
                    if src[i] == b'"' {
                        out[i] = b'_';
                    }
                    i += 1;
                }
                if i < src.len() {
                    out[i] = b'"';
                }
                i += 1;
            }
            b'$' | b':' | b'@' | b'#' => {
                // SQLite's variable loop (tokenize.c, CC_VARALPHA / CC_DOLLAR).
                let mut n = 0;
                i += 1;
                while i < src.len() {
                    let d = src[i];
                    if is_id_char(d) {
                        n += 1;
                    } else if d == b'(' && n > 0 {
                        let open = i;
                        loop {
                            i += 1;
                            if i >= src.len() || is_space(src[i]) || src[i] == b')' {
                                break;
                            }
                        }
                        if i < src.len() && src[i] == b')' {
                            i += 1;
                        } else {
                            unterminated = true;
                        }
                        for b in &mut out[open..i] {
                            *b = b'_';
                        }
                        break;
                    } else if d == b':' && src.get(i + 1) == Some(&b':') {
                        i += 1;
                    } else {
                        break;
                    }
                    i += 1;
                }
            }
            _ if c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80 => {
                let start = i;
                while i < src.len()
                    && (is_id_char(src[i]) || (c.is_ascii_digit() && src[i] == b'.'))
                {
                    i += 1;
                }
                if !c.is_ascii_digit() {
                    // Token edges are ASCII or the text's ends: always char boundaries.
                    if let Some(w) = text.get(start..i) {
                        on_word(w);
                    }
                }
            }
            b'(' => {
                depth += 1;
                i += 1;
            }
            b')' => {
                depth -= 1;
                unterminated |= depth < 0;
                i += 1;
            }
            _ => i += 1,
        }
    }
    // An unterminated token runs to the end and steps past it.
    unterminated |= i > src.len() || depth != 0;
    // Only whole tokens were rewritten with ASCII bytes; fall back to the original otherwise.
    let view = String::from_utf8(out).unwrap_or_else(|_| text.to_owned());
    (view, unterminated)
}

/// `true` when `text` starts (after blanks, comments and opening parentheses) with `WITH`.
fn starts_with_with(view: &str) -> bool {
    let mut rest = view.trim_start();
    loop {
        if let Some(r) = rest.strip_prefix("--") {
            rest = r.split_once('\n').map_or("", |(_, r)| r).trim_start();
        } else if let Some(r) = rest.strip_prefix("/*") {
            rest = r.split_once("*/").map_or("", |(_, r)| r).trim_start();
        } else if let Some(r) = rest.strip_prefix('(') {
            rest = r.trim_start();
        } else {
            break;
        }
    }
    rest.get(..4)
        .is_some_and(|w| w.eq_ignore_ascii_case("with"))
        && !rest.as_bytes().get(4).copied().is_some_and(is_id_char)
}

/// `true` when a DML keyword is a whole token in code, as SQLite's tokenizer reads the raw
/// text: a bare word bounded by non-identifier characters (`is_id_char`: so `updated_at` is
/// not `UPDATE`), outside strings, quoted identifiers, variables and comments (so
/// `SELECT 'delete'` is not a write). This is a second, independent tokenization: the shared
/// lexer never sees it.
///
/// Incomplete text (an unterminated token or unbalanced parentheses: SQLite refuses it, so
/// nothing would run) falls back to whole words anywhere in the raw text, strings and comments
/// included.
fn mentions_dml(text: &str) -> bool {
    let is_dml = |w: &str| {
        ["INSERT", "UPDATE", "DELETE", "REPLACE"]
            .iter()
            .any(|k| w.eq_ignore_ascii_case(k))
    };
    let mut found = false;
    let (_, unterminated) = scan(text, |w| found |= is_dml(w));
    found
        || (unterminated
            && text
                .split(|c: char| {
                    !(c.is_ascii_alphanumeric() || c == '_' || c == '$' || !c.is_ascii())
                })
                .any(is_dml))
}

/// Pragmas that only read the schema, with an argument (`PRAGMA table_info(t)`) or as the
/// table-valued function `pragma_table_info('t')`. Shared with the read-only authorizer.
pub(crate) const PRAGMAS_WITH_ARG: &[&str] = &[
    "table_info",
    "table_xinfo",
    "table_list",
    "index_list",
    "index_info",
    "index_xinfo",
    "foreign_key_list",
    "foreign_key_check",
    "integrity_check",
    "quick_check",
];

/// Pragmas that read a setting when run without a value. Shared with the read-only authorizer.
pub(crate) const PRAGMAS_READ: &[&str] = &[
    "query_only",
    "database_list",
    "collation_list",
    "compile_options",
    "function_list",
    "module_list",
    "pragma_list",
    "schema_version",
    "user_version",
    "application_id",
    "data_version",
    "encoding",
    "page_size",
    "page_count",
    "freelist_count",
    "foreign_keys",
    "busy_timeout",
    "cache_size",
    "journal_mode",
    "auto_vacuum",
    "cell_size_check",
];

/// `true` for exactly `PRAGMA [schema.]name` with `name` in [`PRAGMAS_READ`] or
/// [`PRAGMAS_WITH_ARG`], or `PRAGMA [schema.]name(arg)` with `name` in [`PRAGMAS_WITH_ARG`] and
/// `arg` a plain name, number or simple quoted literal. Anything else (comments, `=`, quoted
/// pragma names, a second statement) is not recognised, so the shared classifier's verdict
/// stands.
fn is_read_pragma(text: &str) -> bool {
    let b = text.trim().trim_end_matches(';').trim_end().as_bytes();
    let mut i = 0;
    let word = |i: &mut usize| -> Option<String> {
        let start = *i;
        while *i < b.len() && (b[*i].is_ascii_alphanumeric() || b[*i] == b'_') {
            *i += 1;
        }
        (*i > start && !b[start].is_ascii_digit())
            .then(|| String::from_utf8_lossy(&b[start..*i]).to_ascii_lowercase())
    };
    let ws = |i: &mut usize| {
        while *i < b.len() && b[*i].is_ascii_whitespace() {
            *i += 1;
        }
    };
    if word(&mut i).as_deref() != Some("pragma") || !b.get(i).is_some_and(u8::is_ascii_whitespace) {
        return false;
    }
    ws(&mut i);
    let Some(mut name) = word(&mut i) else {
        return false;
    };
    if b.get(i) == Some(&b'.') {
        i += 1;
        match word(&mut i) {
            Some(n) => name = n,
            None => return false,
        }
    }
    ws(&mut i);
    if i == b.len() {
        return PRAGMAS_READ.contains(&name.as_str()) || PRAGMAS_WITH_ARG.contains(&name.as_str());
    }
    if b[i] != b'(' || !PRAGMAS_WITH_ARG.contains(&name.as_str()) {
        return false;
    }
    i += 1;
    ws(&mut i);
    match b.get(i) {
        Some(q @ (b'\'' | b'"')) => {
            let q = *q;
            i += 1;
            while i < b.len() && b[i] != q {
                i += 1;
            }
            if i == b.len() {
                return false;
            }
            i += 1;
        }
        Some(c) if c.is_ascii_alphanumeric() || *c == b'_' => {
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
        }
        _ => return false,
    }
    ws(&mut i);
    if b.get(i) != Some(&b')') {
        return false;
    }
    i += 1;
    ws(&mut i);
    i == b.len()
}

impl Dialect for SqliteDialect {
    fn lexical_rules(&self) -> LexicalRules {
        LexicalRules::SQLITE
    }

    fn split_statements(&self, script: &str) -> Vec<StatementSpan> {
        split_with_rules(&lexer_view(script), LexicalRules::SQLITE)
    }

    fn classify(&self, text: &str) -> StatementClass {
        let view = lexer_view(text);
        let class = classify_with_rules(&view, LexicalRules::SQLITE);
        // The shared classifier knows a few schema pragmas only; the reading forms the
        // read-only authorizer allows are reads here too.
        if class == StatementClass::ChangesAccessMode && is_read_pragma(&view) {
            return StatementClass::Read;
        }
        // Belt and braces: a `WITH` statement that mentions a DML keyword anywhere (even in a
        // string or a comment the lexers might disagree about) is never a mere read.
        if class <= StatementClass::Session && starts_with_with(&view) && mentions_dml(text) {
            return StatementClass::Unknown;
        }
        class
    }

    fn explain(&self, query: &str, format: ExplainFormat) -> Option<String> {
        match format {
            ExplainFormat::Text => Some(format!(
                "EXPLAIN QUERY PLAN {}",
                query.trim().trim_end_matches(';')
            )),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ninesql_driver_api::conformance;
    use ninesql_driver_api::ensure_single_statement;
    use proptest::prelude::*;

    #[test]
    fn brackets_become_quotes_of_the_same_length() {
        assert_eq!(lexer_view("SELECT [a;b]"), "SELECT \"a;b\"");
        assert_eq!(lexer_view("SELECT [a\"b]"), "SELECT \"a_b\"");
        assert_eq!(lexer_view("SELECT '[x'"), "SELECT '[x'");
        assert_eq!(lexer_view("SELECT \"[x\""), "SELECT \"[x\"");
        assert_eq!(lexer_view("-- [x\nSELECT 1"), "-- [x\nSELECT 1");
        assert_eq!(lexer_view("/* [ */ SELECT 1"), "/* [ */ SELECT 1");
        assert_eq!(lexer_view("SELECT [unterminated"), "SELECT \"unterminated");
    }

    #[test]
    fn bracket_identifiers_cannot_hide_statements() {
        let d = SqliteDialect;
        // A quote inside brackets would open a string for an ANSI lexer and swallow the `;`.
        let sql = "SELECT [it's] FROM t; DROP TABLE t";
        assert_eq!(d.split_statements(sql).len(), 2);
        assert!(ensure_single_statement(&d, sql).is_err());
        assert_eq!(d.classify(sql), StatementClass::Destructive);
        // A `;` inside brackets is part of the name.
        assert_eq!(d.split_statements("SELECT [a;b] FROM t").len(), 1);
        assert_eq!(
            d.classify("PRAGMA [query_only] = OFF"),
            StatementClass::ChangesAccessMode
        );
    }

    #[test]
    fn sqlite_statements_classify_conservatively() {
        let d = SqliteDialect;
        for sql in conformance::SQLITE_ACCESS_MODE_WRITES {
            assert!(d.classify(sql).is_write(), "{sql}");
        }
        for sql in [
            "VACUUM",
            "VACUUM INTO '/tmp/x.db'",
            "ANALYZE",
            "REINDEX",
            "DETACH DATABASE other",
            "SELECT load_extension('evil')",
            "PRAGMA temp_store = MEMORY",
            "PRAGMA journal_mode = WAL",
            "CREATE TEMP TABLE t (a)",
            "REPLACE INTO t VALUES (1)",
            "INSERT OR REPLACE INTO t VALUES (1)",
        ] {
            assert!(d.classify(sql).is_write(), "{sql}");
        }
        for sql in [
            "SELECT * FROM t",
            "PRAGMA table_info(t)",
            "EXPLAIN QUERY PLAN SELECT 1",
            "WITH c AS (SELECT 1) SELECT * FROM c",
        ] {
            assert_eq!(d.classify(sql), StatementClass::Read, "{sql}");
        }
        assert_eq!(d.classify("BEGIN IMMEDIATE"), StatementClass::Transaction);
    }

    #[test]
    fn reading_pragmas_are_reads() {
        let d = SqliteDialect;
        for sql in [
            "PRAGMA query_only",
            "pragma main.query_only;",
            "PRAGMA integrity_check",
            "PRAGMA quick_check(10)",
            "PRAGMA table_xinfo('customers')",
            "PRAGMA main.index_list( \"t\" )",
            "PRAGMA foreign_key_check",
        ] {
            assert_eq!(d.classify(sql), StatementClass::Read, "{sql}");
        }
        for sql in [
            "PRAGMA query_only = OFF",
            "PRAGMA query_only(0)",
            "PRAGMA query_only = 1",
            "PRAGMA \"query_only\" = 0",
            "PRAGMA [query_only] = 0",
            "PRAGMA user_version = 1",
            "PRAGMA journal_mode(WAL)",
            "PRAGMA query_only /* c */",
            "PRAGMA query_only; PRAGMA query_only = 0",
            "PRAGMA table_info(t); DROP TABLE t",
            "PRAGMA optimize",
        ] {
            assert!(d.classify(sql).is_write(), "{sql}");
        }
    }

    #[test]
    fn quoting_roundtrips() {
        conformance::assert_dialect_quoting_roundtrip(
            &SqliteDialect,
            conformance::ALIAS_TEMPLATE,
            conformance::IDENTIFIER_SAMPLES,
        );
        assert_eq!(
            SqliteDialect
                .explain("SELECT 1;", ExplainFormat::Text)
                .as_deref(),
            Some("EXPLAIN QUERY PLAN SELECT 1")
        );
        assert!(
            SqliteDialect
                .explain("SELECT 1", ExplainFormat::Json)
                .is_none()
        );
    }

    /// Security review H1: Tcl-style variables swallow quotes and comment markers in SQLite.
    #[test]
    fn tcl_variables_cannot_hide_a_write() {
        let d = SqliteDialect;
        for sql in [
            "WITH c AS (SELECT $a(')) DELETE FROM t WHERE b <> '--'",
            "WITH c AS (SELECT :a(')) DELETE FROM t WHERE b <> '--'",
            "WITH c AS (SELECT @a(')) DELETE FROM t WHERE b <> '--'",
            "WITH c AS (SELECT #a(')) DELETE FROM t WHERE b <> '--'",
            "WITH c AS (SELECT $a::b(')) DELETE FROM t WHERE b <> '--'",
            "WITH c AS (SELECT $a(')) UPDATE t SET b = 1 WHERE b <> '--'",
            "WITH c AS (SELECT $a([)) DELETE FROM t WHERE b <> ']'",
            "WITH c AS (SELECT $a(/*)) DELETE FROM t WHERE b <> '*/'",
        ] {
            let class = d.classify(sql);
            assert!(class.is_write(), "{sql} -> {class:?}");
            // The lexer view alone already fixes it (without the `WITH` safety net).
            let lexed = classify_with_rules(&lexer_view(sql), LexicalRules::SQLITE);
            assert!(lexed.is_write(), "{sql} -> {lexed:?}");
            assert_eq!(d.split_statements(sql).len(), 1, "{sql}");
        }
        assert_eq!(lexer_view("SELECT $a(')) x"), "SELECT $a___) x");
        // A `;` inside the variable's parentheses is not a separator for SQLite either.
        assert_eq!(d.split_statements("SELECT $a(;) FROM t").len(), 1);
        // `a$b(` is a function call, not a variable.
        assert_eq!(lexer_view("SELECT a$b(')')"), "SELECT a$b(')')");
        // Plain variables keep reads as reads.
        for sql in ["SELECT $a, :b, @c, #d, ?1 FROM t", "SELECT $a(x) FROM t"] {
            assert_eq!(d.classify(sql), StatementClass::Read, "{sql}");
        }
    }

    /// Re-review follow-up: the `WITH` safety net matches whole DML tokens in code only.
    #[test]
    fn with_safety_net_matches_whole_tokens_in_code() {
        let d = SqliteDialect;
        for sql in [
            "WITH x AS (SELECT updated_at FROM t) SELECT * FROM x",
            "WITH x AS (SELECT 1) SELECT 'delete' FROM x",
            "WITH x AS (SELECT 1) SELECT \"insert\", [replace], $v FROM x -- delete",
            "WITH x AS (SELECT inserted, deleted_by, x_update FROM t) SELECT * FROM x",
        ] {
            assert_eq!(d.classify(sql), StatementClass::Read, "{sql}");
        }
        assert!(mentions_dml("WITH x AS (SELECT 1) DeLeTe FROM t"));
        assert!(mentions_dml(
            "WITH x AS (SELECT $a(')) delete FROM t WHERE b <> '--'"
        ));
        assert!(!mentions_dml(
            "SELECT 'update', updated FROM t /* insert */"
        ));
        // Unterminated text: whole words anywhere.
        assert!(mentions_dml(
            "WITH c AS (SELECT ') INSERT INTO t VALUES (1)"
        ));
        assert!(!mentions_dml("WITH c AS (SELECT ') SELECT updated_at"));
        assert!(mentions_dml(
            "WITH c AS (SELECT `) INSERT INTO t VALUES (1) `"
        ));
        assert!(!mentions_dml("WITH c AS (SELECT (1)) SELECT 'insert'"));
    }

    thread_local! {
        static ORACLE: rusqlite::Connection = {
            let c = rusqlite::Connection::open_in_memory().unwrap();
            c.execute_batch("CREATE TABLE t (a, b)").unwrap();
            c
        };
    }

    fn junk() -> impl Strategy<Value = String> {
        let frag = prop::sample::select(vec![
            "$a(", ":a(", "@a(", "#a(", "$a::b(", "'", "\"", "`", "[", "]", ")", "(", "--", "/*",
            "*/", ";", " ", "\n", "x", "1", "$", ":", "::", "e'", "X'00'", "a$b(",
        ]);
        prop::collection::vec(frag, 0..12).prop_map(|v| v.concat())
    }

    proptest! {
        /// A `WITH` whose main statement is a write never classifies as a read, whatever
        /// lexical junk precedes it.
        #[test]
        fn with_dml_is_never_read(
            j in junk(),
            dml in prop::sample::select(vec![
                "INSERT INTO t VALUES (1)",
                "UPDATE t SET b = 1",
                "DELETE FROM t",
                "REPLACE INTO t VALUES (1)",
            ]),
            tail in junk(),
        ) {
            let sql = format!("WITH c AS (SELECT {j}) {dml} {tail}");
            let class = SqliteDialect.classify(&sql);
            // SQLite is the oracle: junk can comment out or quote the DML, or leave the text
            // incomplete, and then nothing writes. Whenever SQLite compiles a statement that
            // writes, the classifier must not call it a read.
            let writes = ORACLE.with(|c| match c.prepare(&sql) {
                Ok(st) => !st.readonly(),
                Err(_) => false,
            });
            if writes {
                prop_assert!(class > StatementClass::Session, "{} -> {:?}", sql, class);
            }
            // Without junk the DML is always the main statement.
            let clean = format!("WITH c AS (SELECT 1) {dml}");
            prop_assert!(SqliteDialect.classify(&clean) > StatementClass::Session);
        }

        #[test]
        fn rewrite_keeps_length_and_boundaries(text in "\\PC{0,200}") {
            let out = lexer_view(&text);
            prop_assert_eq!(out.len(), text.len());
            for (i, _) in text.char_indices() {
                prop_assert!(out.is_char_boundary(i));
            }
        }
    }
}
