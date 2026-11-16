//! SQL generation used by the transfer: literals for the "SQL INSERT" export and parameterized
//! multi-row `INSERT` statements for the CSV import.
//!
//! # P5-B integration point
//! P5-B (`driver-engineer`) adds DML generation with dialect quoting and typed literal rendering
//! to `ninesql-driver-api`. Until it lands, both traits below are implemented on top of the
//! existing [`Dialect`] methods ([`Dialect::quote_identifier`], [`Dialect::qualified_name`],
//! [`Dialect::format_literal`], [`Dialect::placeholder`]) by [`DialectSql`]. After P5-B merges,
//! implement [`SqlLiteralRenderer`] and [`InsertRenderer`] with its renderer (or replace the
//! bodies of [`DialectSql`]'s implementations) and keep the tests of this module and of
//! `tests/export_formats.rs` passing: nothing else in the transfer code depends on how a literal
//! or a statement is spelled.

use std::sync::Arc;

use ninesql_driver_api::{Dialect, Param};

use crate::error::TransferError;
use crate::value::CellValue;

/// Renders SQL literals and names for the "SQL INSERT" export. Output is for a file the user
/// reads or runs later, never executed by NineSQL.
pub trait SqlLiteralRenderer: Send + Sync {
    /// A quoted, qualified table name.
    fn table_name(&self, parts: &[String]) -> String;

    /// A quoted column name.
    fn column_name(&self, name: &str) -> String;

    /// The literal of `value`. Must stay one SQL token sequence whatever the content
    /// (quotes, backslashes, comment markers, control characters are escaped).
    ///
    /// # Errors
    /// [`TransferError::Unsupported`] for a value the dialect cannot spell.
    fn literal(&self, value: &CellValue<'_>) -> Result<String, TransferError>;
}

/// Builds the parameterized `INSERT` of the CSV import.
pub trait InsertRenderer: Send + Sync {
    /// `INSERT INTO <table> (<columns>) VALUES (<placeholders>), …` for `rows` rows, with
    /// placeholders numbered from 1 in row-major order.
    ///
    /// # Errors
    /// [`TransferError::InvalidOption`] for an empty table, no columns or no rows.
    fn insert(
        &self,
        table: &[String],
        columns: &[String],
        rows: usize,
    ) -> Result<String, TransferError>;
}

/// [`SqlLiteralRenderer`] and [`InsertRenderer`] over a session's or driver's [`Dialect`]
/// (interim implementation until P5-B, see the [module documentation](self)).
#[derive(Clone)]
pub struct DialectSql {
    dialect: Arc<dyn Dialect>,
}

impl std::fmt::Debug for DialectSql {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DialectSql").finish_non_exhaustive()
    }
}

impl DialectSql {
    /// Wraps a dialect.
    #[must_use]
    pub fn new(dialect: Arc<dyn Dialect>) -> Self {
        Self { dialect }
    }
}

/// Converts a cell into the bind / literal parameter of the same type.
///
/// # Errors
/// [`TransferError::Unsupported`] for dates and timestamps out of range and for Arrow types the
/// exporter does not render. Unsigned integers above `i64::MAX` become decimals.
pub fn cell_to_param(value: &CellValue<'_>) -> Result<Param, TransferError> {
    Ok(match value {
        CellValue::Null => Param::Null,
        CellValue::Bool(b) => Param::Bool(*b),
        CellValue::Int(i) => Param::Int(*i),
        CellValue::UInt(u) => match i64::try_from(*u) {
            Ok(i) => Param::Int(i),
            Err(_) => Param::Decimal(u.to_string()),
        },
        CellValue::Float(f) => Param::Float(*f),
        CellValue::Decimal(d) => Param::Decimal(d.clone()),
        CellValue::Text(s) => Param::Text((*s).to_owned()),
        CellValue::Bytes(b) => Param::Bytes(b.to_vec()),
        CellValue::Date(days) => Param::Date(
            i32::try_from(*days)
                .map_err(|_| TransferError::Unsupported("date out of range".into()))?,
        ),
        CellValue::Time { secs, nanos } => {
            let mut s = format!(
                "{:02}:{:02}:{:02}",
                secs / 3600,
                (secs % 3600) / 60,
                secs % 60
            );
            if *nanos != 0 {
                s.push_str(&format!(".{:06}", nanos / 1000));
            }
            Param::Text(s)
        }
        CellValue::Timestamp { secs, nanos, utc } => Param::Timestamp {
            micros: secs
                .checked_mul(1_000_000)
                .and_then(|m| m.checked_add(i64::from(nanos / 1000)))
                .ok_or_else(|| TransferError::Unsupported("timestamp out of range".into()))?,
            utc: *utc,
        },
        CellValue::Duration(s) => Param::Text(s.clone()),
        CellValue::Unsupported(label) => {
            return Err(TransferError::Unsupported(format!(
                "values of type {label} cannot be written as SQL literals"
            )));
        }
    })
}

impl SqlLiteralRenderer for DialectSql {
    fn table_name(&self, parts: &[String]) -> String {
        // P5-B integration point: table quoting.
        self.dialect.qualified_name(parts)
    }

    fn column_name(&self, name: &str) -> String {
        // P5-B integration point: identifier quoting.
        self.dialect.quote_identifier(name)
    }

    fn literal(&self, value: &CellValue<'_>) -> Result<String, TransferError> {
        // P5-B integration point: typed literal rendering.
        let param = cell_to_param(value)?;
        let text = self.dialect.format_literal(&param);
        // `format_literal` quotes text; a control character stays inside the quotes. Refuse a
        // NUL, which several engines cannot read back from a script.
        if text.contains('\0') {
            return Err(TransferError::Unsupported(
                "a text value holds a NUL character".into(),
            ));
        }
        Ok(text)
    }
}

impl InsertRenderer for DialectSql {
    fn insert(
        &self,
        table: &[String],
        columns: &[String],
        rows: usize,
    ) -> Result<String, TransferError> {
        // P5-B integration point: parameterized INSERT generation.
        if table.is_empty() || table.iter().any(String::is_empty) {
            return Err(TransferError::InvalidOption("no target table".into()));
        }
        if columns.is_empty() {
            return Err(TransferError::InvalidOption("no mapped column".into()));
        }
        if rows == 0 {
            return Err(TransferError::InvalidOption("no row to insert".into()));
        }
        let mut out = String::with_capacity(32 + columns.len() * (rows * 6 + 16));
        out.push_str("INSERT INTO ");
        out.push_str(&self.dialect.qualified_name(table));
        out.push_str(" (");
        for (i, c) in columns.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            out.push_str(&self.dialect.quote_identifier(c));
        }
        out.push_str(") VALUES ");
        let mut n = 0usize;
        for r in 0..rows {
            if r > 0 {
                out.push_str(", ");
            }
            out.push('(');
            for c in 0..columns.len() {
                if c > 0 {
                    out.push_str(", ");
                }
                n += 1;
                out.push_str(&self.dialect.placeholder(n));
            }
            out.push(')');
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use ninesql_driver_api::AnsiDialect;

    use super::*;
    use crate::value::days_from_civil as iso_days;

    #[test]
    fn insert_shape_and_quoting() {
        let sql = DialectSql::new(Arc::new(AnsiDialect));
        let text = sql
            .insert(
                &["s".into(), "t\"x".into()],
                &["a".into(), "b; DROP".into()],
                2,
            )
            .unwrap();
        assert_eq!(
            text,
            "INSERT INTO \"s\".\"t\"\"x\" (\"a\", \"b; DROP\") VALUES (?, ?), (?, ?)"
        );
        assert!(sql.insert(&[], &["a".into()], 1).is_err());
        assert!(sql.insert(&["t".into()], &[], 1).is_err());
    }

    #[test]
    fn literals_escape() {
        let sql = DialectSql::new(Arc::new(AnsiDialect));
        assert_eq!(
            sql.literal(&CellValue::Text("it's'); DROP TABLE x; --"))
                .unwrap(),
            "'it''s''); DROP TABLE x; --'"
        );
        assert_eq!(sql.literal(&CellValue::Null).unwrap(), "NULL");
        assert_eq!(
            sql.literal(&CellValue::Bytes(&[0, 255])).unwrap(),
            "X'00FF'"
        );
        assert_eq!(
            sql.literal(&CellValue::Date(iso_days(2024, 1, 31)))
                .unwrap(),
            "DATE '2024-01-31'"
        );
        assert!(sql.literal(&CellValue::Text("a\0b")).is_err());
        assert!(sql.literal(&CellValue::Unsupported("List".into())).is_err());
    }
}
