//! SQL generation used by the transfer: the typed `INSERT` text of the "SQL INSERT" export and
//! the parameterized multi-row `INSERT` statements of the CSV import.
//!
//! Both are built on the P5-B DML renderer of `ninesql-driver-api` ([`ninesql_driver_api::dml`]):
//! - the export renders each row with [`insert_preview`] (dialect quoting, identifier checks,
//!   typed literals that read the same in every lexical mode of the engine, and the self-check
//!   that the text is exactly one `INSERT` the dialect classifies as a filtered write), reading
//!   the cells with [`cell_value`] and typing them with [`DmlColumn::from_field`]; a multi-row
//!   statement joins the `VALUES` tuples of rows that each passed that check. A value the
//!   driver cut at the value budget is refused rather than written cut;
//! - the import's statement is checked the same way: every name passes
//!   [`Dialect::dml_check_identifier`], and the text must split into one statement and classify
//!   as [`StatementClass::Write`].

use std::sync::Arc;

use ninesql_driver_api::arrow_array::RecordBatch;
use ninesql_driver_api::arrow_schema::Schema;
use ninesql_driver_api::dml::{ColumnValue, DmlColumn, DmlError, cell_value, insert_preview};
use ninesql_driver_api::{Dialect, StatementClass};

use crate::error::TransferError;

/// Renders the "SQL INSERT" export. Output is for a file the user reads or runs later, never
/// executed by NineSQL.
pub trait SqlLiteralRenderer: Send + Sync {
    /// `INSERT INTO <table> (<columns>) VALUES` (no trailing blank) for a result with `schema`.
    ///
    /// # Errors
    /// [`TransferError::InvalidOption`] for an empty table or no column;
    /// [`TransferError::Unsupported`] for a name the engine would reject or change.
    fn insert_prefix(&self, table: &[String], schema: &Schema) -> Result<String, TransferError>;

    /// The `(<literal>, …)` tuple of row `row` of `batch`, typed by the batch's own fields.
    ///
    /// # Errors
    /// [`TransferError::Unsupported`] for a value the dialect cannot spell faithfully, a value
    /// the driver cut at the value budget (writing it would lose data), or a failed self-check.
    fn row_values(
        &self,
        table: &[String],
        batch: &RecordBatch,
        row: usize,
    ) -> Result<String, TransferError>;
}

/// Builds the parameterized `INSERT` of the CSV import.
pub trait InsertRenderer: Send + Sync {
    /// `INSERT INTO <table> (<columns>) VALUES (<placeholders>), …` for `rows` rows, with
    /// placeholders numbered from 1 in row-major order.
    ///
    /// # Errors
    /// [`TransferError::InvalidOption`] for an empty table, no columns or no rows;
    /// [`TransferError::Unsupported`] for a name the engine would reject or change, or a failed
    /// self-check.
    fn insert(
        &self,
        table: &[String],
        columns: &[String],
        rows: usize,
    ) -> Result<String, TransferError>;
}

/// [`SqlLiteralRenderer`] and [`InsertRenderer`] over a session's or driver's [`Dialect`], with
/// the P5-B DML renderer (see the [module documentation](self)).
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

    fn check_table(&self, table: &[String]) -> Result<String, TransferError> {
        if table.is_empty() || table.iter().any(String::is_empty) {
            return Err(TransferError::InvalidOption("no target table".into()));
        }
        for part in table {
            self.dialect.dml_check_identifier(part).map_err(dml_error)?;
        }
        Ok(self.dialect.qualified_name(table))
    }

    fn column_list<'a>(
        &self,
        names: impl Iterator<Item = &'a str>,
    ) -> Result<String, TransferError> {
        let mut out = String::new();
        for (i, name) in names.enumerate() {
            self.dialect.dml_check_identifier(name).map_err(dml_error)?;
            if i > 0 {
                out.push_str(", ");
            }
            out.push_str(&self.dialect.quote_identifier(name));
        }
        if out.is_empty() {
            return Err(TransferError::InvalidOption("no column".into()));
        }
        Ok(out)
    }
}

/// A DML generation error as a transfer error (names columns, never values).
fn dml_error(e: DmlError) -> TransferError {
    TransferError::Unsupported(e.to_string())
}

impl SqlLiteralRenderer for DialectSql {
    fn insert_prefix(&self, table: &[String], schema: &Schema) -> Result<String, TransferError> {
        let table = self.check_table(table)?;
        let columns = self.column_list(schema.fields().iter().map(|f| f.name().as_str()))?;
        Ok(format!("INSERT INTO {table} ({columns}) VALUES"))
    }

    fn row_values(
        &self,
        table: &[String],
        batch: &RecordBatch,
        row: usize,
    ) -> Result<String, TransferError> {
        let schema = batch.schema();
        if schema.fields().is_empty() {
            return Err(TransferError::InvalidOption("no column".into()));
        }
        let values = schema
            .fields()
            .iter()
            .enumerate()
            .map(|(i, field)| {
                Ok(ColumnValue::new(
                    DmlColumn::from_field(field),
                    cell_value(batch, i, row).map_err(dml_error)?,
                ))
            })
            .collect::<Result<Vec<_>, TransferError>>()?;
        let text = insert_preview(&*self.dialect, table, &values).map_err(dml_error)?;
        // The preview is `INSERT INTO <table> (<columns>) VALUES (<literals>)`; the prefix is
        // rebuilt with the same quoting, so what follows it is exactly the tuple.
        let prefix = format!("{} ", self.insert_prefix(table, &schema)?);
        match text.strip_prefix(&prefix) {
            Some(tuple) if tuple.starts_with('(') && tuple.ends_with(')') => Ok(tuple.to_owned()),
            _ => Err(TransferError::Unsupported(
                "the generated INSERT does not have the expected shape".into(),
            )),
        }
    }
}

impl InsertRenderer for DialectSql {
    fn insert(
        &self,
        table: &[String],
        columns: &[String],
        rows: usize,
    ) -> Result<String, TransferError> {
        let table = self.check_table(table)?;
        if columns.is_empty() {
            return Err(TransferError::InvalidOption("no mapped column".into()));
        }
        if rows == 0 {
            return Err(TransferError::InvalidOption("no row to insert".into()));
        }
        let column_list = self.column_list(columns.iter().map(String::as_str))?;
        let mut out = String::with_capacity(32 + columns.len() * (rows * 6 + 16));
        out.push_str("INSERT INTO ");
        out.push_str(&table);
        out.push_str(" (");
        out.push_str(&column_list);
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
        // The same self-check as the P5-B generator: one statement, a filtered write.
        if self.dialect.split_statements(&out).len() != 1
            || self.dialect.classify(&out) != StatementClass::Write
        {
            return Err(TransferError::Unsupported(
                "the generated INSERT failed a safety check".into(),
            ));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use ninesql_driver_api::AnsiDialect;
    use ninesql_driver_api::arrow_array::{ArrayRef, BinaryArray, Date32Array, StringArray};
    use ninesql_driver_api::arrow_schema::{DataType, Field};

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
        assert!(sql.insert(&["t".into()], &["a\0b".into()], 1).is_err());
    }

    fn one_row(field: Field, array: ArrayRef) -> RecordBatch {
        RecordBatch::try_new(Arc::new(Schema::new(vec![field])), vec![array]).unwrap()
    }

    fn literal(sql: &DialectSql, batch: &RecordBatch) -> Result<String, TransferError> {
        sql.row_values(&["t".into()], batch, 0)
    }

    #[test]
    fn literals_escape() {
        let sql = DialectSql::new(Arc::new(AnsiDialect));
        let text = |v: Option<&str>| {
            one_row(
                Field::new("v", DataType::Utf8, true),
                Arc::new(StringArray::from(vec![v])),
            )
        };
        assert_eq!(
            literal(&sql, &text(Some("it's'); DROP TABLE x; --"))).unwrap(),
            "('it''s''); DROP TABLE x; --')"
        );
        assert_eq!(literal(&sql, &text(None)).unwrap(), "(NULL)");
        let bytes = one_row(
            Field::new("v", DataType::Binary, true),
            Arc::new(BinaryArray::from(vec![&[0u8, 255][..]])),
        );
        assert_eq!(literal(&sql, &bytes).unwrap().to_lowercase(), "(x'00ff')");
        let date = one_row(
            Field::new("v", DataType::Date32, true),
            Arc::new(Date32Array::from(vec![
                i32::try_from(iso_days(2024, 1, 31)).unwrap(),
            ])),
        );
        assert_eq!(literal(&sql, &date).unwrap(), "(DATE '2024-01-31')");
        assert!(literal(&sql, &text(Some("a\0b"))).is_err());
        assert_eq!(
            sql.insert_prefix(&["s".into(), "t".into()], &text(None).schema())
                .unwrap(),
            "INSERT INTO \"s\".\"t\" (\"v\") VALUES"
        );
        // A value cut at the fetch budget is never written as a literal.
        let cut = one_row(
            Field::new("v", DataType::Utf8, true).with_metadata([(
                ninesql_driver_api::TRUNCATED_VALUES_METADATA_KEY.to_owned(),
                "true".to_owned(),
            )]),
            Arc::new(StringArray::from(vec![Some("abc")])),
        );
        assert!(literal(&sql, &cut).is_err());
    }
}
