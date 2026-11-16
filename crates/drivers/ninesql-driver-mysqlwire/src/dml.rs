//! MySQL-family DML literals (P5-B, docs/08 "DML generation"): MySQL, MariaDB, TiDB.
//!
//! The executed statement's parameters are inlined by the driver as hexadecimal literals
//! ([`crate::dialect`], ADR-0020 §2). The preview writes the same values readably, with the same
//! types: digits, `…e…` doubles, `X'…'` bytes, `DATE '…'` / `TIMESTAMP '…'`, and strings as
//! `'…'` escaped for the session's `NO_BACKSLASH_ESCAPES` (`''` always, `\\` when backslashes
//! are escapes). A string holding a character the preview must not show raw (controls,
//! invisible or bidirectional characters) is written `CONVERT(X'…' USING utf8mb4)`, exactly as
//! the bound form, except `\0`, `\n`, `\r`, `\t`, `\b` and `\Z` when backslashes are escapes.
//! `ANSI_QUOTES` does not matter: identifiers are always backquoted and strings single-quoted.

use ninesql_driver_api::Param;
use ninesql_driver_api::arrow_schema::DataType;
use ninesql_driver_api::dml::{
    ColumnType, DmlError, check_identifier_basic, date_text, hex_lower, is_plain_decimal,
    needs_escape, timestamp_text,
};

/// MySQL, MariaDB and TiDB identifiers have at most 64 characters.
const MAX_IDENTIFIER_CHARS: usize = 64;

/// Types without a reliable `=` against a literal: JSON compares as JSON values (a string
/// literal is a JSON string), spatial types compare as binary geometry, vectors are opaque, and
/// `BIT` compared with a hexadecimal literal is read as a number (an error in strict mode).
const NOT_COMPARABLE: &[&str] = &[
    "bit",
    "geometry",
    "geometrycollection",
    "geomcollection",
    "json",
    "linestring",
    "multilinestring",
    "multipoint",
    "multipolygon",
    "point",
    "polygon",
    "vector",
];

/// [`Dialect::dml_comparable`](ninesql_driver_api::Dialect::dml_comparable) for the family.
///
/// `FLOAT` (Arrow `Float32`) is not comparable: the text protocol sends it with about six
/// significant digits, so the value read back is not the stored one and `f = <read value>`
/// misses the row.
pub(crate) fn comparable(ty: &ColumnType) -> bool {
    if ty.data_type == DataType::Float32 {
        return false;
    }
    !ty.engine_type_lower().is_some_and(|t| {
        let base = t.split(['(', ' ']).next().unwrap_or("");
        NOT_COMPARABLE.contains(&base)
    })
}

/// [`Dialect::dml_check_identifier`](ninesql_driver_api::Dialect::dml_check_identifier).
pub(crate) fn check_identifier(name: &str) -> Result<(), DmlError> {
    check_identifier_basic(name)?;
    if name.chars().count() > MAX_IDENTIFIER_CHARS {
        return Err(DmlError::invalid_identifier(
            name,
            "longer than 64 characters",
        ));
    }
    if name.chars().any(|c| u32::from(c) > 0xFFFF) {
        return Err(DmlError::invalid_identifier(
            name,
            "characters outside the Basic Multilingual Plane are not allowed in names",
        ));
    }
    Ok(())
}

/// The string literal of `s` for the session's backslash mode.
pub(crate) fn string_literal(s: &str, backslash_escapes: bool) -> String {
    let named = |c: char| matches!(c, '\0' | '\n' | '\r' | '\t' | '\u{8}' | '\u{1a}');
    let hex = s
        .chars()
        .any(|c| (c == '\0' || needs_escape(c)) && !(backslash_escapes && named(c)));
    if hex {
        return format!("CONVERT(X'{}' USING utf8mb4)", hex_lower(s.as_bytes()));
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        match c {
            '\'' => out.push_str("''"),
            '\\' if backslash_escapes => out.push_str("\\\\"),
            '\0' => out.push_str("\\0"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{1a}' => out.push_str("\\Z"),
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

/// The typed literal of `value`.
pub(crate) fn literal(value: &Param, backslash_escapes: bool) -> Result<String, &'static str> {
    Ok(match value {
        Param::Null => "NULL".to_owned(),
        Param::Bool(b) => if *b { "TRUE" } else { "FALSE" }.to_owned(),
        Param::Int(i) => i.to_string(),
        // The bound form's spelling (`1.5e0`): a DOUBLE literal, not a DECIMAL one.
        Param::Float(f) if f.is_finite() => format!("{f:e}"),
        Param::Float(_) => return Err("MySQL has no NaN or infinite floating-point values"),
        Param::Decimal(d) if is_plain_decimal(d) => d.clone(),
        Param::Decimal(_) => return Err("not a plain decimal number"),
        Param::Text(s) | Param::Json(s) => string_literal(s, backslash_escapes),
        Param::Bytes(b) => format!("X'{}'", hex_lower(b)),
        Param::Date(d) => format!("DATE '{}'", date_text(*d)?),
        Param::Timestamp { utc: true, .. } => {
            return Err("MySQL has no time-zone-aware timestamp literal");
        }
        Param::Timestamp { micros, .. } => format!("TIMESTAMP '{}'", timestamp_text(*micros)?),
        _ => return Err("unsupported value type"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings_per_mode() {
        assert_eq!(string_literal("it's", true), "'it''s'");
        assert_eq!(string_literal("a\\b", true), "'a\\\\b'");
        assert_eq!(string_literal("a\\b", false), "'a\\b'");
        assert_eq!(string_literal("a\nb", true), "'a\\nb'");
        assert_eq!(
            string_literal("a\nb", false),
            "CONVERT(X'610a62' USING utf8mb4)"
        );
        assert_eq!(
            string_literal("\u{202e}", true),
            "CONVERT(X'e280ae' USING utf8mb4)"
        );
        assert_eq!(string_literal("é", false), "'é'");
        assert_eq!(string_literal("", false), "''");
    }

    #[test]
    fn values() {
        assert_eq!(literal(&Param::Float(1.5), true).unwrap(), "1.5e0");
        assert_eq!(literal(&Param::Int(-3), true).unwrap(), "-3");
        assert!(literal(&Param::Float(f64::NAN), true).is_err());
        assert!(
            literal(
                &Param::Timestamp {
                    micros: 0,
                    utc: true
                },
                true
            )
            .is_err()
        );
        assert!(literal(&Param::Decimal("1;".into()), true).is_err());
        assert_eq!(literal(&Param::Bytes(vec![]), true).unwrap(), "X''");
    }

    #[test]
    fn identifiers_and_comparability() {
        assert!(check_identifier(&"é".repeat(64)).is_ok());
        assert!(check_identifier(&"é".repeat(65)).is_err());
        assert!(check_identifier("a\u{1F600}").is_err());
        let json = ColumnType::new(DataType::Utf8).with_engine_type("json");
        assert!(!comparable(&json));
        assert!(comparable(
            &ColumnType::new(DataType::Utf8).with_engine_type("decimal")
        ));
    }
}
