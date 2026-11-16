//! MySQL-family DML literals (P5-B, docs/08 "DML generation"): MySQL, MariaDB, TiDB.
//!
//! The executed statement's parameters are inlined by the driver as hexadecimal literals
//! ([`crate::dialect`], ADR-0020 §2). The preview writes the same values readably, with the same
//! types: digits, `…e…` doubles, `X'…'` bytes, `DATE '…'` / `TIMESTAMP '…'`, and strings as
//! `'…'` with `'` doubled. Every literal means the same in every `sql_mode` and client
//! character set, so a preview or an export rendered in one session can be replayed in another
//! (security review H1): a string holding a backslash, a NUL, a control, invisible or
//! bidirectional character is written `CONVERT(X'…' USING utf8mb4)`, exactly as the bound form.
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

/// The string literal of `s`, with the same meaning in every `sql_mode` and client character
/// set (security review H1): a string holding a backslash, a NUL or a character
/// [`needs_escape`] flags is written `CONVERT(X'…' USING utf8mb4)` (the bound form); any other
/// string is `'…'` with `'` doubled, which reads the same with and without
/// `NO_BACKSLASH_ESCAPES` and holds no byte a multi-byte client character set (GBK, SJIS, Big5,
/// CP932) could pair with a quote.
pub(crate) fn string_literal(s: &str) -> String {
    let hex = s.chars().any(|c| c == '\\' || c == '\0' || needs_escape(c));
    if hex {
        return format!("CONVERT(X'{}' USING utf8mb4)", hex_lower(s.as_bytes()));
    }
    format!("'{}'", s.replace('\'', "''"))
}

/// The typed literal of `value`.
pub(crate) fn literal(value: &Param) -> Result<String, &'static str> {
    Ok(match value {
        Param::Null => "NULL".to_owned(),
        Param::Bool(b) => if *b { "TRUE" } else { "FALSE" }.to_owned(),
        Param::Int(i) => i.to_string(),
        // The bound form's spelling (`1.5e0`): a DOUBLE literal, not a DECIMAL one.
        Param::Float(f) if f.is_finite() => format!("{f:e}"),
        Param::Float(_) => return Err("MySQL has no NaN or infinite floating-point values"),
        Param::Decimal(d) if is_plain_decimal(d) => d.clone(),
        Param::Decimal(_) => return Err("not a plain decimal number"),
        Param::Text(s) | Param::Json(s) => string_literal(s),
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
    fn strings_are_mode_independent() {
        assert_eq!(string_literal("it's"), "'it''s'");
        assert_eq!(string_literal("a\\b"), "CONVERT(X'615c62' USING utf8mb4)");
        assert_eq!(
            string_literal("\\'' -- '"),
            "CONVERT(X'5c2727202d2d2027' USING utf8mb4)"
        );
        assert_eq!(string_literal("a\nb"), "CONVERT(X'610a62' USING utf8mb4)");
        assert_eq!(
            string_literal("\u{202e}"),
            "CONVERT(X'e280ae' USING utf8mb4)"
        );
        assert_eq!(string_literal("é"), "'é'");
        assert_eq!(string_literal(""), "''");
    }

    #[test]
    fn values() {
        assert_eq!(literal(&Param::Float(1.5)).unwrap(), "1.5e0");
        assert_eq!(literal(&Param::Int(-3)).unwrap(), "-3");
        assert!(literal(&Param::Float(f64::NAN)).is_err());
        assert!(
            literal(&Param::Timestamp {
                micros: 0,
                utc: true
            })
            .is_err()
        );
        assert!(literal(&Param::Decimal("1;".into())).is_err());
        assert_eq!(literal(&Param::Bytes(vec![])).unwrap(), "X''");
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
