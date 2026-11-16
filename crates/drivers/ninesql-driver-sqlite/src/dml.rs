//! SQLite DML literals (P5-B, docs/08 "DML generation").
//!
//! SQLite is dynamically typed: a value's storage class depends on how it was written, then on
//! the column's affinity. The preview therefore writes each value with the storage class the
//! driver binds it with ([`crate::worker`]): booleans as `1` / `0`, integers and finite floats
//! as numbers (infinities as `9e999` / `-9e999`), decimals, text, JSON, dates and timestamps as
//! **text** (`'12.50'`, `'2024-01-01'`, `'… .ffffff'` with `Z` for UTC), bytes as `X'…'`.
//! SQLite has no backslash escapes: `'` is doubled, and a string holding a character the
//! preview must not show raw is written as a concatenation with `char(…)`, in parentheses
//! (`('a' || char(10) || 'b')`). NaN is refused, as the driver refuses to bind it.

use std::fmt::Write as _;

use ninesql_driver_api::Param;
use ninesql_driver_api::dml::{date_text, float_text, hex_lower, needs_escape, timestamp_text};

/// The string literal of `s`.
pub(crate) fn string_literal(s: &str) -> String {
    if !s.chars().any(|c| c == '\0' || needs_escape(c)) {
        return format!("'{}'", s.replace('\'', "''"));
    }
    let mut parts: Vec<String> = Vec::new();
    let mut plain = String::new();
    let mut codes: Vec<u32> = Vec::new();
    let flush_codes = |codes: &mut Vec<u32>, parts: &mut Vec<String>| {
        if !codes.is_empty() {
            let mut call = String::from("char(");
            for (i, c) in codes.iter().enumerate() {
                if i > 0 {
                    call.push_str(", ");
                }
                let _ = write!(call, "{c}");
            }
            call.push(')');
            parts.push(call);
            codes.clear();
        }
    };
    for c in s.chars() {
        if c == '\0' || needs_escape(c) {
            if !plain.is_empty() {
                parts.push(format!("'{}'", plain.replace('\'', "''")));
                plain.clear();
            }
            codes.push(u32::from(c));
        } else {
            flush_codes(&mut codes, &mut parts);
            plain.push(c);
        }
    }
    flush_codes(&mut codes, &mut parts);
    if !plain.is_empty() {
        parts.push(format!("'{}'", plain.replace('\'', "''")));
    }
    format!("({})", parts.join(" || "))
}

/// The literal of `value`, with the storage class the driver binds it with.
pub(crate) fn literal(value: &Param) -> Result<String, &'static str> {
    Ok(match value {
        Param::Null => "NULL".to_owned(),
        Param::Bool(b) => if *b { "1" } else { "0" }.to_owned(),
        Param::Int(i) => i.to_string(),
        Param::Float(f) if f.is_nan() => return Err("SQLite stores NaN as NULL"),
        Param::Float(f) if f.is_infinite() => if *f > 0.0 { "9e999" } else { "-9e999" }.to_owned(),
        Param::Float(f) => {
            let t = float_text(*f);
            // A bare integer spelling would be an INTEGER literal.
            if t.contains(['.', 'e', 'E']) {
                t
            } else {
                format!("{t}.0")
            }
        }
        Param::Decimal(s) | Param::Text(s) | Param::Json(s) => string_literal(s),
        Param::Bytes(b) => format!("X'{}'", hex_lower(b)),
        Param::Date(d) => format!("'{}'", date_text(*d)?),
        Param::Timestamp { micros, utc } => {
            format!(
                "'{}{}'",
                timestamp_text(*micros)?,
                if *utc { "Z" } else { "" }
            )
        }
        _ => return Err("unsupported value type"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings() {
        assert_eq!(string_literal("it's"), "'it''s'");
        assert_eq!(string_literal("a\\b"), "'a\\b'");
        assert_eq!(string_literal("a\nb"), "('a' || char(10) || 'b')");
        assert_eq!(string_literal("\0\r'"), "(char(0, 13) || '''')");
        assert_eq!(string_literal(""), "''");
    }

    #[test]
    fn values() {
        assert_eq!(literal(&Param::Bool(true)).unwrap(), "1");
        assert_eq!(literal(&Param::Float(1e300)).unwrap(), "1e300");
        assert_eq!(literal(&Param::Float(2.0)).unwrap(), "2.0");
        assert_eq!(literal(&Param::Float(f64::INFINITY)).unwrap(), "9e999");
        assert!(literal(&Param::Float(f64::NAN)).is_err());
        assert_eq!(literal(&Param::Decimal("12.50".into())).unwrap(), "'12.50'");
        assert_eq!(literal(&Param::Date(0)).unwrap(), "'1970-01-01'");
    }
}
