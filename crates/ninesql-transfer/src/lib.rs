//! Data transfer (ROADMAP P5-C): streamed export of results to CSV, TSV, JSON, JSON Lines, SQL
//! `INSERT`, Markdown and HTML (F-XFER-01), and the engine-independent part of the CSV import
//! (F-XFER-03), both with bounded memory (I6).
//!
//! - [`export`]: [`Exporter`] writes result items row by row in one of the [`ExportFormat`]s,
//!   with escaping that keeps values from injecting into the output format and an on-by-default
//!   spreadsheet formula guard for CSV / TSV.
//! - [`file`]: [`OutputFile`], the export file: a new private file written next to the target
//!   and moved into place at the end, never through a symbolic link, never over an existing file
//!   without confirmation, removed on cancel or error (I4).
//! - [`csv`]: [`CsvReader`], a streaming CSV reader with file-size, record-length and
//!   field-count caps.
//! - [`import`]: target types, the column mapping, typed conversion into bind parameters, the
//!   preview and the [`RowSource`] that batches the file.
//! - [`sql`]: the SQL renderers of the "SQL INSERT" export and of the import's parameterized
//!   `INSERT`s, behind traits for the P5-B integration point.
//! - [`value`]: typed access to Arrow values and their text forms.
//!
//! The session side (running the statement, transactions, confirmation, cancel, progress) lives
//! in `ninesql-core` (`TransferService`). Nothing here logs a value or a secret (I1); errors and
//! issues carry positions and reasons only.
#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod csv;
mod error;
pub mod export;
pub mod file;
pub mod import;
pub mod sql;
pub mod value;

pub use csv::{CsvFormat, CsvLimits, CsvReader, Record, RecordError, RecordErrorKind};
pub use error::TransferError;
pub use export::{
    ExportFormat, ExportOptions, ExportStats, Exporter, LineEnding, QuoteStyle, SqlExportOptions,
};
pub use file::{OutputFile, Overwrite};
pub use import::{
    ColumnMapping, ConvertError, CsvPreview, GuessedType, IssueKind, MappedColumn, MappingMode,
    PreviewRow, RowBatch, RowIssue, RowSource, TargetColumn, TargetType, Trim, ValueOptions,
};
pub use sql::{DialectSql, InsertRenderer, SqlLiteralRenderer};
pub use value::{BinaryText, CellValue, TextOptions, TimePattern};
