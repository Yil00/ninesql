//! SQLite dialect: ANSI defaults with SQLite's lexical rules (`` `…` `` and `[…]` identifiers,
//! no backslash escapes, non-nested block comments), `EXPLAIN QUERY PLAN` and `?` parameters.
//!
//! The shared lexer of `ninesql-driver-api` has no switch for `[…]` identifiers (an SQL Server
//! habit SQLite accepts). Before splitting or classifying, [`bracket_idents_as_quotes`] rewrites
//! every `[…]` identifier that starts in code (not inside a string, a quoted identifier or a
//! comment) into a `"…"` identifier of the same byte length, so offsets stay valid and a `;` or
//! a quote inside brackets cannot hide a statement from the classifier.

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

/// Rewrites `[…]` identifiers that start in code into `"…"` identifiers of the same length:
/// `[` and the closing `]` become `"`, and a `"` inside the brackets becomes `_` (SQLite has
/// no escape inside brackets: the first `]` ends the identifier). An unterminated `[` runs to
/// the end of the text, as in SQLite's tokenizer. Strings (`'…'`, `x'…'`), `"…"` and `` `…` ``
/// identifiers, `--` and `/* … */` comments are copied unchanged.
#[must_use]
pub fn bracket_idents_as_quotes(text: &str) -> String {
    let src = text.as_bytes();
    if !src.contains(&b'[') {
        return text.to_owned();
    }
    let mut out = src.to_vec();
    let mut i = 0;
    while i < src.len() {
        match src[i] {
            q @ (b'\'' | b'"' | b'`') => {
                // Quoted token: a doubled quote stays inside it.
                i += 1;
                while i < src.len() {
                    if src[i] == q {
                        if src.get(i + 1) == Some(&q) {
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
            _ => i += 1,
        }
    }
    // Only ASCII bytes were replaced by ASCII bytes, so the text stays valid UTF-8.
    String::from_utf8(out).unwrap_or_else(|_| text.to_owned())
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
        split_with_rules(&bracket_idents_as_quotes(script), LexicalRules::SQLITE)
    }

    fn classify(&self, text: &str) -> StatementClass {
        let rewritten = bracket_idents_as_quotes(text);
        let class = classify_with_rules(&rewritten, LexicalRules::SQLITE);
        // The shared classifier knows a few schema pragmas only; the reading forms the
        // read-only authorizer allows are reads here too.
        if class == StatementClass::ChangesAccessMode && is_read_pragma(&rewritten) {
            StatementClass::Read
        } else {
            class
        }
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
        assert_eq!(bracket_idents_as_quotes("SELECT [a;b]"), "SELECT \"a;b\"");
        assert_eq!(bracket_idents_as_quotes("SELECT [a\"b]"), "SELECT \"a_b\"");
        assert_eq!(bracket_idents_as_quotes("SELECT '[x'"), "SELECT '[x'");
        assert_eq!(bracket_idents_as_quotes("SELECT \"[x\""), "SELECT \"[x\"");
        assert_eq!(
            bracket_idents_as_quotes("-- [x\nSELECT 1"),
            "-- [x\nSELECT 1"
        );
        assert_eq!(
            bracket_idents_as_quotes("/* [ */ SELECT 1"),
            "/* [ */ SELECT 1"
        );
        assert_eq!(
            bracket_idents_as_quotes("SELECT [unterminated"),
            "SELECT \"unterminated"
        );
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

    proptest! {
        #[test]
        fn rewrite_keeps_length_and_boundaries(text in "\\PC{0,200}") {
            let out = bracket_idents_as_quotes(&text);
            prop_assert_eq!(out.len(), text.len());
            for (i, _) in text.char_indices() {
                prop_assert!(out.is_char_boundary(i));
            }
        }
    }
}
