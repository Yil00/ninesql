//! PostgreSQL dialect: quoting, literals, `$n` placeholders, explain, and the lexical rules kept
//! in sync with the session's `standard_conforming_strings` (docs/08, ADR-0004).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use ninesql_driver_api::{Dialect, ExplainFormat, LexicalRules, Param};

/// The PostgreSQL dialect.
///
/// The lexical rules start from [`LexicalRules::POSTGRES`] (`standard_conforming_strings = on`,
/// `E'…'` strings, dollar quotes, nested comments, `$` inside identifiers). A session dialect
/// shares the session's `standard_conforming_strings` flag, updated from every `ParameterStatus`
/// message, so the classifier and the splitter follow the session: with the setting off, a
/// backslash escapes the next character in every string literal.
#[derive(Clone, Debug)]
pub struct PgDialect {
    standard_conforming_strings: Arc<AtomicBool>,
}

impl Default for PgDialect {
    fn default() -> Self {
        Self::new()
    }
}

impl PgDialect {
    /// The dialect of a server with `standard_conforming_strings = on` (the default since 9.1).
    #[must_use]
    pub fn new() -> Self {
        Self {
            standard_conforming_strings: Arc::new(AtomicBool::new(true)),
        }
    }

    /// A dialect that follows a session's `standard_conforming_strings` flag.
    #[must_use]
    pub(crate) fn tracking(flag: Arc<AtomicBool>) -> Self {
        Self {
            standard_conforming_strings: flag,
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from(DIGITS[usize::from(b >> 4)]));
        s.push(char::from(DIGITS[usize::from(b & 0x0f)]));
    }
    s
}

impl Dialect for PgDialect {
    fn lexical_rules(&self) -> LexicalRules {
        let scs = self.standard_conforming_strings.load(Ordering::SeqCst);
        LexicalRules::POSTGRES.with_backslash_escapes(!scs)
    }

    fn format_literal(&self, value: &Param) -> String {
        match value {
            // `X'…'` is a bit string in PostgreSQL; `decode` is independent of
            // `standard_conforming_strings`.
            Param::Bytes(b) => format!("decode('{}', 'hex')", hex(b)),
            Param::Timestamp { utc: true, .. } => {
                let ansi = ninesql_driver_api::AnsiDialect.format_literal(value);
                // `TIMESTAMP '… +00:00'` would drop the offset: use `timestamptz`.
                ansi.replacen("TIMESTAMP", "TIMESTAMPTZ", 1)
            }
            other => AnsiWithRules(self.lexical_rules()).format_literal(other),
        }
    }

    fn placeholder(&self, index: usize) -> String {
        format!("${index}")
    }

    fn explain(&self, query: &str, format: ExplainFormat) -> Option<String> {
        let q = query.trim().trim_end_matches(';');
        Some(match format {
            ExplainFormat::Text => format!("EXPLAIN {q}"),
            ExplainFormat::Json => format!("EXPLAIN (FORMAT JSON) {q}"),
            ExplainFormat::Analyze => format!("EXPLAIN ANALYZE {q}"),
            _ => return None,
        })
    }
}

/// The ANSI defaults under given lexical rules (literal formatting honours backslash escapes).
struct AnsiWithRules(LexicalRules);

impl Dialect for AnsiWithRules {
    fn lexical_rules(&self) -> LexicalRules {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ninesql_driver_api::{StatementClass, conformance, ensure_single_statement};

    #[test]
    fn quoting_roundtrip() {
        conformance::assert_dialect_quoting_roundtrip(
            &PgDialect::new(),
            conformance::ALIAS_TEMPLATE,
            conformance::IDENTIFIER_SAMPLES,
        );
    }

    #[test]
    fn placeholders_and_explain() {
        let d = PgDialect::new();
        assert_eq!(d.placeholder(3), "$3");
        assert_eq!(
            d.explain("SELECT 1;", ExplainFormat::Json).as_deref(),
            Some("EXPLAIN (FORMAT JSON) SELECT 1")
        );
        assert_eq!(
            d.explain("SELECT 1", ExplainFormat::Analyze).as_deref(),
            Some("EXPLAIN ANALYZE SELECT 1")
        );
    }

    #[test]
    fn literals() {
        let d = PgDialect::new();
        assert_eq!(
            d.format_literal(&Param::Bytes(vec![0xde, 0xad])),
            "decode('dead', 'hex')"
        );
        assert!(
            d.format_literal(&Param::Timestamp {
                micros: 0,
                utc: true
            })
            .starts_with("TIMESTAMPTZ '1970-01-01 00:00:00")
        );
        assert_eq!(d.format_literal(&Param::Text("a\\b".into())), "'a\\b'");
    }

    #[test]
    fn standard_conforming_strings_drive_the_lexer() {
        let flag = Arc::new(AtomicBool::new(true));
        let d = PgDialect::tracking(Arc::clone(&flag));
        // With scs on, `\` is an ordinary character: the quote closes the literal.
        let sql = r"SELECT 'a\'; DELETE FROM t; --'";
        assert_eq!(d.split_statements(sql).len(), 2);
        assert!(ensure_single_statement(&d, sql).is_err());
        flag.store(false, Ordering::SeqCst);
        // With scs off, `\'` escapes the quote: one statement holding a string.
        assert_eq!(d.split_statements(sql).len(), 1);
        assert_eq!(d.classify(sql), StatementClass::Read);
        assert_eq!(d.format_literal(&Param::Text("a\\b".into())), "'a\\\\b'");
        assert!(d.lexical_rules().backslash_escapes);
    }
}
