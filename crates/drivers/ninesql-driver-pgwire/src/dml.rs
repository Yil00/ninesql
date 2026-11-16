//! PostgreSQL-family DML literals (P5-B, docs/08 "DML generation"): PostgreSQL, TimescaleDB,
//! YugabyteDB (QuestDB declares no editing; its export text uses the same rules).
//!
//! The executed statement binds every value in text format with an unspecified type (except
//! `bytea`), so the server reads it with the **column's** input function. The preview mirrors
//! that exactly: the same text, quoted, cast to the column's type when that type is a built-in
//! name of [`BUILTIN_TYPES`] (never a name copied from an unknown engine type), or left untyped
//! (an `unknown` literal, resolved to the column's type exactly as the parameter is). Numbers
//! and booleans in a numeric or boolean column are written bare.
//!
//! Strings with a backslash, a control character or an invisible character use `E'…'`, whose
//! meaning does not depend on `standard_conforming_strings`; a NUL is refused (PostgreSQL text
//! cannot hold it). `bytea` is `'\x…'::bytea`, or `E'\\x…'::bytea` when
//! `standard_conforming_strings` is off.

use std::fmt::Write as _;

use ninesql_driver_api::Param;
use ninesql_driver_api::arrow_schema::DataType;
use ninesql_driver_api::dml::{
    ColumnType, DmlError, check_identifier_basic, date_text, float_text, hex_lower,
    is_plain_decimal, needs_escape, timestamp_text,
};

/// PostgreSQL keeps identifiers up to `NAMEDATALEN - 1` bytes and silently truncates longer
/// quoted names: a longer name would target another column.
const MAX_IDENTIFIER_BYTES: usize = 63;

/// Built-in type names a preview literal may be cast to (lower case, as `format_type` and the
/// driver spell them). Not `character` / `bpchar` / `bit` (a cast without a length truncates to
/// one character or bit) nor `"char"`.
const BUILTIN_TYPES: &[&str] = &[
    "bigint",
    "bigserial",
    "bit varying",
    "bool",
    "boolean",
    "box",
    "bytea",
    "character varying",
    "cidr",
    "circle",
    "date",
    "daterange",
    "decimal",
    "double precision",
    "float4",
    "float8",
    "inet",
    "int",
    "int2",
    "int4",
    "int4range",
    "int8",
    "int8range",
    "integer",
    "interval",
    "json",
    "jsonb",
    "line",
    "lseg",
    "macaddr",
    "macaddr8",
    "money",
    "name",
    "numeric",
    "numrange",
    "oid",
    "path",
    "point",
    "polygon",
    "real",
    "smallint",
    "text",
    "time",
    "time with time zone",
    "time without time zone",
    "timestamp",
    "timestamp with time zone",
    "timestamp without time zone",
    "timestamptz",
    "timetz",
    "tsquery",
    "tsrange",
    "tstzrange",
    "tsvector",
    "uuid",
    "varbit",
    "varchar",
    "xml",
];

/// Numeric types: integers, floats and decimals are written bare in these columns.
const NUMERIC_TYPES: &[&str] = &[
    "bigint",
    "decimal",
    "double precision",
    "float4",
    "float8",
    "int",
    "int2",
    "int4",
    "int8",
    "integer",
    "numeric",
    "oid",
    "real",
    "smallint",
];

/// Types without a reliable `=`: `json` and `xml` have no equality operator, the geometric
/// types compare areas or approximate positions.
const NOT_COMPARABLE: &[&str] = &[
    "box", "circle", "json", "line", "lseg", "path", "point", "polygon", "xml",
];

/// The type a preview literal is cast to: the column's built-in engine type (typmod dropped,
/// array suffixes kept), else the type of the Arrow mapping, else none.
pub(crate) fn cast_type(ty: &ColumnType) -> Option<String> {
    if let Some(t) = ty.engine_type_lower() {
        return builtin(&t);
    }
    Some(
        match &ty.data_type {
            DataType::Boolean => "boolean",
            DataType::Int16 => "smallint",
            DataType::Int32 => "integer",
            DataType::Int64 => "bigint",
            DataType::UInt32 => "oid",
            DataType::Float32 => "real",
            DataType::Float64 => "double precision",
            DataType::Binary | DataType::LargeBinary => "bytea",
            DataType::Date32 => "date",
            DataType::Time64(_) => "time without time zone",
            DataType::Timestamp(_, None) => "timestamp without time zone",
            DataType::Timestamp(_, Some(_)) => "timestamp with time zone",
            _ => return None,
        }
        .to_owned(),
    )
}

/// `name` when it is a built-in type name, optionally with a typmod (dropped) and `[]`
/// suffixes (kept).
fn builtin(name: &str) -> Option<String> {
    let mut base = name.trim();
    let mut dims = 0usize;
    while let Some(b) = base.strip_suffix("[]") {
        base = b.trim_end();
        dims += 1;
    }
    // A typmod: `numeric(10,2)`, `character varying(20)`, `timestamp(3) with time zone` is not
    // handled (no cast).
    if let Some(open) = base.find('(') {
        let inner = base[open..].strip_prefix('(')?.strip_suffix(')')?;
        if !inner
            .bytes()
            .all(|b| b.is_ascii_digit() || b == b',' || b == b' ')
        {
            return None;
        }
        base = base[..open].trim_end();
    }
    let base = BUILTIN_TYPES.iter().find(|t| **t == base)?;
    Some(format!("{base}{}", "[]".repeat(dims)))
}

fn base_name(ty: &ColumnType) -> Option<String> {
    let t = cast_type(ty)?;
    Some(t.trim_end_matches("[]").to_owned())
}

/// `true` when values of `ty` are written bare.
fn numeric(ty: &ColumnType) -> bool {
    !is_array(ty) && base_name(ty).is_some_and(|b| NUMERIC_TYPES.contains(&b.as_str()))
}

fn is_array(ty: &ColumnType) -> bool {
    cast_type(ty).is_some_and(|t| t.ends_with("[]"))
}

fn boolean(ty: &ColumnType) -> bool {
    !is_array(ty) && base_name(ty).is_some_and(|b| b == "boolean" || b == "bool")
}

/// [`Dialect::dml_comparable`](ninesql_driver_api::Dialect::dml_comparable) for the family.
pub(crate) fn comparable(ty: &ColumnType) -> bool {
    let name = ty
        .engine_type_lower()
        .map(|t| t.trim_end_matches("[]").trim().to_owned());
    !name.is_some_and(|n| {
        let n = n.split('(').next().unwrap_or("").trim().to_owned();
        NOT_COMPARABLE.contains(&n.as_str())
    })
}

/// [`Dialect::dml_check_identifier`](ninesql_driver_api::Dialect::dml_check_identifier).
pub(crate) fn check_identifier(name: &str) -> Result<(), DmlError> {
    check_identifier_basic(name)?;
    if name.len() > MAX_IDENTIFIER_BYTES {
        return Err(DmlError::invalid_identifier(
            name,
            "longer than 63 bytes: PostgreSQL would truncate it",
        ));
    }
    Ok(())
}

/// A string literal that reads back as `s` whatever `standard_conforming_strings` is.
pub(crate) fn string_literal(s: &str) -> Result<String, &'static str> {
    if s.contains('\0') {
        return Err("PostgreSQL text cannot hold a NUL character");
    }
    let escape = s.chars().any(|c| c == '\\' || needs_escape(c));
    let mut out = String::with_capacity(s.len() + 3);
    out.push_str(if escape { "E'" } else { "'" });
    for c in s.chars() {
        match c {
            '\'' => out.push_str("''"),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if needs_escape(c) => {
                let cp = u32::from(c);
                // `\u` above U+007F needs a UTF-8 server encoding; any other fails, never
                // changes the value.
                let _ = if cp > 0xFFFF {
                    write!(out, "\\U{cp:08X}")
                } else {
                    write!(out, "\\u{cp:04X}")
                };
            }
            c => out.push(c),
        }
    }
    out.push('\'');
    Ok(out)
}

fn quoted_with_cast(text: &str, ty: &ColumnType) -> Result<String, &'static str> {
    let lit = string_literal(text)?;
    Ok(match cast_type(ty) {
        Some(t) => format!("{lit}::{t}"),
        None => lit,
    })
}

/// The typed literal of `value` for a column of type `ty`.
pub(crate) fn literal(
    value: &Param,
    ty: &ColumnType,
    standard_conforming_strings: bool,
) -> Result<String, &'static str> {
    Ok(match value {
        Param::Null => "NULL".to_owned(),
        Param::Bool(b) if boolean(ty) => if *b { "TRUE" } else { "FALSE" }.to_owned(),
        Param::Bool(b) => quoted_with_cast(if *b { "t" } else { "f" }, ty)?,
        Param::Int(i) if numeric(ty) => i.to_string(),
        Param::Int(i) => quoted_with_cast(&i.to_string(), ty)?,
        // `-0.0` written bare would be the numeric zero, losing the sign.
        Param::Float(f) if f.is_finite() && numeric(ty) && !(*f == 0.0 && f.is_sign_negative()) => {
            float_text(*f)
        }
        Param::Float(f) => {
            let text = if f.is_nan() {
                "NaN".to_owned()
            } else if f.is_infinite() {
                if *f > 0.0 { "Infinity" } else { "-Infinity" }.to_owned()
            } else {
                float_text(*f)
            };
            quoted_with_cast(&text, ty)?
        }
        Param::Decimal(d) if numeric(ty) && is_plain_decimal(d) => d.clone(),
        Param::Decimal(d) => quoted_with_cast(d, ty)?,
        Param::Text(s) | Param::Json(s) => quoted_with_cast(s, ty)?,
        Param::Bytes(b) => {
            if standard_conforming_strings {
                format!("'\\x{}'::bytea", hex_lower(b))
            } else {
                format!("E'\\\\x{}'::bytea", hex_lower(b))
            }
        }
        Param::Date(d) => quoted_with_cast(&date_text(*d)?, ty)?,
        Param::Timestamp { micros, utc } => {
            let mut text = timestamp_text(*micros)?;
            if *utc {
                text.push_str("+00");
            }
            quoted_with_cast(&text, ty)?
        }
        _ => return Err("unsupported value type"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ty(engine: &str) -> ColumnType {
        ColumnType::new(DataType::Utf8).with_engine_type(engine)
    }

    #[test]
    fn casts_only_builtin_names() {
        assert_eq!(cast_type(&ty("uuid")).as_deref(), Some("uuid"));
        assert_eq!(cast_type(&ty("integer[]")).as_deref(), Some("integer[]"));
        assert_eq!(cast_type(&ty("numeric(10,2)")).as_deref(), Some("numeric"));
        assert_eq!(cast_type(&ty("character(3)")), None);
        assert_eq!(cast_type(&ty("bit")), None);
        assert_eq!(cast_type(&ty("uuid; drop table t")), None);
        assert_eq!(cast_type(&ty("my_enum")), None);
        assert_eq!(cast_type(&ty("numeric(1); x")), None);
        assert_eq!(
            cast_type(&ColumnType::new(DataType::Int32)).as_deref(),
            Some("integer")
        );
        assert_eq!(cast_type(&ColumnType::new(DataType::Utf8)), None);
    }

    #[test]
    fn literals() {
        let int = ColumnType::new(DataType::Int32);
        let text = ColumnType::new(DataType::Utf8);
        assert_eq!(literal(&Param::Int(-5), &int, true).unwrap(), "-5");
        assert_eq!(literal(&Param::Int(5), &text, true).unwrap(), "'5'");
        assert_eq!(
            literal(&Param::Text("it's".into()), &ty("uuid"), true).unwrap(),
            "'it''s'::uuid"
        );
        assert_eq!(
            literal(&Param::Text("a\\b\n\u{202E}".into()), &text, false).unwrap(),
            "E'a\\\\b\\n\\u202E'"
        );
        assert!(literal(&Param::Text("a\0".into()), &text, true).is_err());
        assert_eq!(
            literal(&Param::Bytes(vec![0, 0xff]), &text, true).unwrap(),
            "'\\x00ff'::bytea"
        );
        assert_eq!(
            literal(&Param::Bytes(vec![0, 0xff]), &text, false).unwrap(),
            "E'\\\\x00ff'::bytea"
        );
        let f8 = ColumnType::new(DataType::Float64);
        assert_eq!(literal(&Param::Float(0.1), &f8, true).unwrap(), "0.1");
        assert_eq!(
            literal(&Param::Float(f64::NAN), &f8, true).unwrap(),
            "'NaN'::double precision"
        );
        assert_eq!(
            literal(&Param::Float(-0.0), &f8, true).unwrap(),
            "'-0.0'::double precision"
        );
        assert_eq!(
            literal(
                &Param::Timestamp {
                    micros: 0,
                    utc: true
                },
                &ColumnType::new(DataType::Timestamp(
                    ninesql_driver_api::arrow_schema::TimeUnit::Microsecond,
                    Some("UTC".into())
                )),
                true
            )
            .unwrap(),
            "'1970-01-01 00:00:00.000000+00'::timestamp with time zone"
        );
        assert_eq!(
            literal(&Param::Decimal("NaN".into()), &ty("numeric"), true).unwrap(),
            "'NaN'::numeric"
        );
        assert_eq!(
            literal(
                &Param::Bool(true),
                &ColumnType::new(DataType::Boolean),
                true
            )
            .unwrap(),
            "TRUE"
        );
        assert_eq!(
            literal(&Param::Bool(true), &ty("text"), true).unwrap(),
            "'t'::text"
        );
    }

    #[test]
    fn identifiers_and_comparability() {
        assert!(check_identifier(&"a".repeat(63)).is_ok());
        assert!(check_identifier(&"a".repeat(64)).is_err());
        assert!(check_identifier("").is_err());
        assert!(check_identifier("a\0b").is_err());
        assert!(!comparable(&ty("json")));
        assert!(!comparable(&ty("json[]")));
        assert!(comparable(&ty("jsonb")));
        assert!(comparable(&ColumnType::new(DataType::Utf8)));
    }
}
