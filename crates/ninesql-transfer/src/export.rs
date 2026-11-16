//! Streamed export of a result to CSV, TSV, JSON (array or JSON Lines), SQL `INSERT`, Markdown
//! and HTML (F-XFER-01).
//!
//! An [`Exporter`] receives the result items of one statement in order ([`Exporter::write_item`])
//! and writes each row to its output as soon as it arrives: it never keeps a batch, only the text
//! of the current row (and, for SQL, nothing more: a multi-row `INSERT` is written row by row).
//! Memory stays at one row plus the output's write buffer whatever the size of the result (I6);
//! the caller holds at most the batch it is passing in.
//!
//! # What is exported
//! The first tabular result set. Later result sets are counted
//! ([`ExportStats::result_sets_skipped`]) and not written; document results are refused
//! ([`TransferError::Unsupported`]). A result the driver cut at its budget
//! ([`ResultItem::Truncated`]) is flagged in [`ExportStats::incomplete`]; values the driver cut
//! at the value budget are reported per column ([`ExportStats::truncated_value_columns`]).
//!
//! # Injection safety
//! - **CSV / TSV:** a field is quoted when it holds the delimiter, the quote, a CR or a LF, and
//!   an empty text is written `""` when the NULL text is empty, so `NULL` and `''` stay distinct.
//!   With [`ExportOptions::formula_guard`] (on by default for CSV and TSV), a text cell or header
//!   that starts with a tab or a CR, or whose first character after leading Unicode whitespace
//!   and format characters is `=`, `+`, `-`, `@`, `|` or a full-width `＝ ＋ － ＠`
//!   ([`needs_formula_guard`]), is prefixed with `'`, so a spreadsheet
//!   opens it as text rather than running it as a formula ("CSV injection"). Numbers are never
//!   prefixed (a negative number stays a number).
//! - **JSON:** strings are escaped per RFC 8259, plus U+2028 / U+2029; keys of duplicate column
//!   names get a `_2`, `_3`… suffix so no value is lost.
//! - **Markdown:** every ASCII punctuation character is backslash-escaped (CommonMark), so a
//!   value cannot open a link, an emphasis, an inline HTML tag or a table cell; line breaks
//!   become `<br>`.
//! - **HTML:** `&`, `<`, `>`, `"` and `'` are escaped in every text; the document carries a
//!   Content-Security-Policy that forbids scripts and remote content.
//! - **SQL:** names and typed literals come from a [`SqlLiteralRenderer`] (the P5-B DML
//!   renderer: dialect quoting, literals that read the same in every lexical mode, a self-check
//!   per row); a value the driver cut at the value budget fails the export instead of being
//!   written cut.

use std::collections::{BTreeSet, HashMap};
use std::io::Write;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ninesql_driver_api::arrow_array::RecordBatch;
use ninesql_driver_api::arrow_schema::Schema;
use ninesql_driver_api::{ResultItem, ResultSetKind, TRUNCATED_VALUES_METADATA_KEY, Truncation};

use crate::error::TransferError;
use crate::sql::SqlLiteralRenderer;
use crate::value::{CellValue, TextOptions, cell, float_text, write_hex};

/// Output format.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ExportFormat {
    /// Comma-separated values (RFC 4180).
    Csv,
    /// Tab-separated values (CSV rules with a tab delimiter).
    Tsv,
    /// One JSON array of objects.
    JsonArray,
    /// JSON Lines: one object per line.
    JsonLines,
    /// SQL `INSERT` statements.
    SqlInsert,
    /// A GitHub-flavoured Markdown table.
    Markdown,
    /// A standalone HTML document with one table.
    Html,
}

impl ExportFormat {
    /// The usual file extension.
    #[must_use]
    pub fn extension(self) -> &'static str {
        match self {
            Self::Csv => "csv",
            Self::Tsv => "tsv",
            Self::JsonArray => "json",
            Self::JsonLines => "jsonl",
            Self::SqlInsert => "sql",
            Self::Markdown => "md",
            Self::Html => "html",
        }
    }
}

/// Line ending.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum LineEnding {
    /// `\n`.
    #[default]
    Lf,
    /// `\r\n` (RFC 4180, spreadsheets on Windows).
    CrLf,
}

impl LineEnding {
    fn as_str(self) -> &'static str {
        match self {
            Self::Lf => "\n",
            Self::CrLf => "\r\n",
        }
    }
}

/// When CSV / TSV fields are quoted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum QuoteStyle {
    /// Only when needed (delimiter, quote, CR, LF; an empty text when the NULL text is empty).
    #[default]
    Necessary,
    /// Every non-NULL field.
    Always,
}

/// Options of the "SQL INSERT" format.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct SqlExportOptions {
    /// Target table of the statements (qualified name parts). Required.
    pub table: Vec<String>,
    /// Rows per `INSERT` statement (1 to 1000, default 1).
    pub rows_per_statement: usize,
}

impl Default for SqlExportOptions {
    fn default() -> Self {
        Self {
            table: Vec::new(),
            rows_per_statement: 1,
        }
    }
}

/// Largest `rows_per_statement`.
pub const MAX_ROWS_PER_STATEMENT: usize = 1000;

/// Export options. Build with [`ExportOptions::new`], which sets the defaults of the format.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ExportOptions {
    /// Format.
    pub format: ExportFormat,
    /// Value text: NULL text, date / time patterns, decimal separator, binary form.
    pub text: TextOptions,
    /// Write a header row (CSV, TSV; Markdown and HTML always have one).
    pub header: bool,
    /// CSV delimiter (TSV always uses a tab).
    pub delimiter: char,
    /// CSV / TSV quote character.
    pub quote: char,
    /// When to quote.
    pub quote_style: QuoteStyle,
    /// Spreadsheet formula guard (see the [module documentation](self)); on by default for CSV
    /// and TSV.
    pub formula_guard: bool,
    /// Start the file with a UTF-8 byte order mark (some spreadsheets need it to read UTF-8).
    /// The output is always UTF-8.
    pub bom: bool,
    /// Line ending.
    pub line_ending: LineEnding,
    /// SQL format options.
    pub sql: SqlExportOptions,
    /// Title of the HTML document.
    pub title: String,
}

impl ExportOptions {
    /// Defaults for `format`: header on, `,` and `"`, formula guard on for CSV / TSV, empty NULL
    /// text for CSV / TSV and `NULL` for Markdown / HTML, LF line endings, no BOM.
    #[must_use]
    pub fn new(format: ExportFormat) -> Self {
        let mut text = TextOptions::default();
        if matches!(format, ExportFormat::Markdown | ExportFormat::Html) {
            text.null_text = "NULL".to_owned();
        }
        Self {
            format,
            text,
            header: true,
            delimiter: if format == ExportFormat::Tsv {
                '\t'
            } else {
                ','
            },
            quote: '"',
            quote_style: QuoteStyle::Necessary,
            formula_guard: matches!(format, ExportFormat::Csv | ExportFormat::Tsv),
            bom: false,
            line_ending: LineEnding::Lf,
            sql: SqlExportOptions::default(),
            title: "NineSQL export".to_owned(),
        }
    }

    /// Checks the options.
    ///
    /// # Errors
    /// [`TransferError::InvalidOption`]: delimiter equal to the quote, a CR / LF delimiter or
    /// quote, no SQL table, `rows_per_statement` out of range, a NULL text with a line break.
    pub fn validate(&self) -> Result<(), TransferError> {
        let bad = |c: char| c == '\r' || c == '\n';
        if matches!(self.format, ExportFormat::Csv | ExportFormat::Tsv) {
            let delimiter = self.effective_delimiter();
            if delimiter == self.quote || bad(delimiter) || bad(self.quote) {
                return Err(TransferError::InvalidOption(
                    "the delimiter and the quote must differ and not be line breaks".into(),
                ));
            }
            let null = &self.text.null_text;
            if null.contains([delimiter, self.quote]) {
                return Err(TransferError::InvalidOption(
                    "the NULL text cannot hold the delimiter or the quote".into(),
                ));
            }
            if self.formula_guard && needs_formula_guard(null) {
                return Err(TransferError::InvalidOption(
                    "the NULL text would be read as a spreadsheet formula".into(),
                ));
            }
            let sep = self.text.decimal_separator;
            if sep == self.quote || bad(sep) {
                return Err(TransferError::InvalidOption(
                    "the decimal separator cannot be the quote or a line break".into(),
                ));
            }
        }
        if self.text.null_text.contains(['\r', '\n']) {
            return Err(TransferError::InvalidOption(
                "the NULL text cannot hold a line break".into(),
            ));
        }
        if self.format == ExportFormat::SqlInsert {
            if self.sql.table.is_empty() || self.sql.table.iter().any(String::is_empty) {
                return Err(TransferError::InvalidOption(
                    "the SQL INSERT format needs a target table name".into(),
                ));
            }
            if !(1..=MAX_ROWS_PER_STATEMENT).contains(&self.sql.rows_per_statement) {
                return Err(TransferError::InvalidOption(
                    "rows per INSERT must be between 1 and 1000".into(),
                ));
            }
        }
        Ok(())
    }

    fn effective_delimiter(&self) -> char {
        if self.format == ExportFormat::Tsv {
            '\t'
        } else {
            self.delimiter
        }
    }
}

/// Counters of an export, also its summary.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ExportStats {
    /// Rows written.
    pub rows: u64,
    /// Bytes written (BOM included).
    pub bytes: u64,
    /// Columns in which the driver cut at least one value at the value budget: the exported
    /// value is the kept prefix.
    pub truncated_value_columns: Vec<String>,
    /// Batches that held at least one cut value.
    pub batches_with_truncated_values: u64,
    /// Further result sets of the statement, not exported.
    pub result_sets_skipped: u32,
    /// The driver stopped the result at its budget: the file holds the rows delivered before.
    pub incomplete: Option<Truncation>,
}

/// Counts bytes written to the inner writer.
struct Counting<W> {
    inner: W,
    bytes: u64,
}

impl<W: Write> Write for Counting<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.bytes += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    /// Before the first tabular result set.
    Waiting,
    /// Writing the first tabular result set.
    Writing,
    /// Past it (later result sets are skipped).
    Done,
}

/// Streams one result into an output (see the [module documentation](self)).
pub struct Exporter<W: Write> {
    out: Counting<W>,
    options: ExportOptions,
    sql: Option<Arc<dyn SqlLiteralRenderer>>,
    state: State,
    header_written: bool,
    columns: Vec<String>,
    json_keys: Vec<String>,
    sql_prefix: String,
    rows: u64,
    rows_in_statement: usize,
    truncated: BTreeSet<String>,
    truncated_batches: u64,
    skipped: u32,
    incomplete: Option<Truncation>,
    line: String,
    started: Instant,
}

impl<W: Write> std::fmt::Debug for Exporter<W> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Exporter")
            .field("format", &self.options.format)
            .field("rows", &self.rows)
            .field("bytes", &self.out.bytes)
            .finish_non_exhaustive()
    }
}

/// Characters that make a spreadsheet read a cell as a formula.
const FORMULA_STARTS: [char; 9] = [
    '=', '+', '-', '@', '|', '\u{FF1D}', '\u{FF0B}', '\u{FF0D}', '\u{FF20}',
];

/// Unicode format characters (category Cf) a spreadsheet may skip before a formula.
fn is_format_char(c: char) -> bool {
    matches!(c,
        '\u{00AD}' | '\u{061C}' | '\u{180E}' | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}'
        | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{206F}' | '\u{FEFF}' | '\u{FFF9}'..='\u{FFFB}')
}

/// `true` when `text` would be read as a formula by a spreadsheet: it starts with a tab or a
/// CR, or its first character after leading Unicode whitespace and format characters is `=`,
/// `+`, `-`, `@`, `|` or a full-width `＝ ＋ － ＠`.
#[must_use]
pub fn needs_formula_guard(text: &str) -> bool {
    if text.starts_with(['\t', '\r']) {
        return true;
    }
    text.chars()
        .find(|c| !c.is_whitespace() && !is_format_char(*c))
        .is_some_and(|c| FORMULA_STARTS.contains(&c))
}

/// Appends `s` as a JSON string (RFC 8259, plus U+2028 / U+2029 escaped).
pub fn write_json_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 || c == '\u{2028}' || c == '\u{2029}' || c == '\u{7f}' => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Appends `s` with HTML special characters escaped.
pub fn write_html_escaped(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            '\0' => out.push('\u{FFFD}'),
            c => out.push(c),
        }
    }
}

/// Appends `s` as the text of a Markdown table cell: ASCII punctuation backslash-escaped, line
/// breaks as `<br>`, other control characters as U+FFFD.
pub fn write_markdown_escaped(out: &mut String, s: &str) {
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push_str("<br>");
            }
            '\n' => out.push_str("<br>"),
            '\t' => out.push(' '),
            c if c.is_ascii_punctuation() => {
                out.push('\\');
                out.push(c);
            }
            c if c.is_control() => out.push('\u{FFFD}'),
            c => out.push(c),
        }
    }
}

impl<W: Write> Exporter<W> {
    /// An exporter writing to `out`. `sql` is required for [`ExportFormat::SqlInsert`].
    ///
    /// # Errors
    /// [`ExportOptions::validate`]; [`TransferError::InvalidOption`] when the SQL format has no
    /// renderer; I/O errors writing the BOM.
    pub fn new(
        out: W,
        options: ExportOptions,
        sql: Option<Arc<dyn SqlLiteralRenderer>>,
    ) -> Result<Self, TransferError> {
        options.validate()?;
        if options.format == ExportFormat::SqlInsert && sql.is_none() {
            return Err(TransferError::InvalidOption(
                "the SQL INSERT format needs a SQL renderer".into(),
            ));
        }
        let mut out = Counting {
            inner: out,
            bytes: 0,
        };
        if options.bom {
            out.write_all("\u{FEFF}".as_bytes())?;
        }
        Ok(Self {
            out,
            options,
            sql,
            state: State::Waiting,
            header_written: false,
            columns: Vec::new(),
            json_keys: Vec::new(),
            sql_prefix: String::new(),
            rows: 0,
            rows_in_statement: 0,
            truncated: BTreeSet::new(),
            truncated_batches: 0,
            skipped: 0,
            incomplete: None,
            line: String::with_capacity(1024),
            started: Instant::now(),
        })
    }

    /// Rows written so far.
    #[must_use]
    pub fn rows(&self) -> u64 {
        self.rows
    }

    /// Bytes written so far (before the output's own buffering).
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.out.bytes
    }

    /// Time since the exporter was created.
    #[must_use]
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// Bytes held by the exporter itself (the current row's text buffer).
    #[must_use]
    pub fn buffered_bytes(&self) -> usize {
        self.line.capacity()
    }

    /// Handles one result item.
    ///
    /// # Errors
    /// [`TransferError::Unsupported`] for documents; I/O errors; SQL literal errors.
    pub fn write_item(&mut self, item: &ResultItem) -> Result<(), TransferError> {
        match item {
            ResultItem::ResultSet(header) => match (&header.kind, self.state) {
                (ResultSetKind::Tabular(schema), State::Waiting) => self.start(schema),
                (ResultSetKind::Documents, State::Waiting) => Err(TransferError::Unsupported(
                    "document results cannot be exported to a table format".into(),
                )),
                (_, State::Writing) => {
                    self.state = State::Done;
                    self.skipped += 1;
                    Ok(())
                }
                (_, State::Done) => {
                    self.skipped += 1;
                    Ok(())
                }
            },
            ResultItem::Rows(batch) if self.state == State::Writing => self.write_batch(batch),
            ResultItem::Truncated(t) => {
                self.incomplete = Some(*t);
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// Starts the result set with `schema` (header row, document prologue).
    ///
    /// # Errors
    /// I/O errors; for the SQL format, a table or column name the dialect refuses.
    pub fn start(&mut self, schema: &Schema) -> Result<(), TransferError> {
        self.state = State::Writing;
        self.columns = schema.fields().iter().map(|f| f.name().clone()).collect();
        self.json_keys = unique_keys(&self.columns);
        if let Some(sql) = self
            .sql
            .as_ref()
            .filter(|_| self.options.format == ExportFormat::SqlInsert)
        {
            self.sql_prefix = sql.insert_prefix(&self.options.sql.table, schema)?;
        }
        self.write_prologue()
    }

    fn eol(&self) -> &'static str {
        self.options.line_ending.as_str()
    }

    fn flush_line(&mut self) -> Result<(), TransferError> {
        self.out.write_all(self.line.as_bytes())?;
        self.line.clear();
        Ok(())
    }

    fn write_prologue(&mut self) -> Result<(), TransferError> {
        if self.header_written {
            return Ok(());
        }
        self.header_written = true;
        let eol = self.eol();
        let mut line = std::mem::take(&mut self.line);
        match self.options.format {
            ExportFormat::Csv | ExportFormat::Tsv => {
                if self.options.header {
                    for (i, name) in self.columns.iter().enumerate() {
                        if i > 0 {
                            line.push(self.options.effective_delimiter());
                        }
                        self.csv_text(&mut line, name);
                    }
                    line.push_str(eol);
                }
            }
            ExportFormat::JsonArray => {
                line.push('[');
            }
            ExportFormat::JsonLines | ExportFormat::SqlInsert => {}
            ExportFormat::Markdown => {
                line.push('|');
                for name in &self.columns {
                    line.push(' ');
                    write_markdown_escaped(&mut line, name);
                    line.push_str(" |");
                }
                line.push_str(eol);
                line.push('|');
                for _ in &self.columns {
                    line.push_str(" --- |");
                }
                line.push_str(eol);
            }
            ExportFormat::Html => {
                line.push_str("<!DOCTYPE html>");
                line.push_str(eol);
                line.push_str("<html><head><meta charset=\"utf-8\">");
                line.push_str("<meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; style-src 'unsafe-inline'\">");
                line.push_str("<title>");
                write_html_escaped(&mut line, &self.options.title);
                line.push_str("</title><style>table{border-collapse:collapse}td,th{border:1px solid #999;padding:2px 6px;white-space:pre-wrap;vertical-align:top}td.null{color:#888;font-style:italic}</style></head>");
                line.push_str(eol);
                line.push_str("<body><table><thead><tr>");
                for name in &self.columns {
                    line.push_str("<th>");
                    write_html_escaped(&mut line, name);
                    line.push_str("</th>");
                }
                line.push_str("</tr></thead>");
                line.push_str(eol);
                line.push_str("<tbody>");
                line.push_str(eol);
            }
        }
        self.line = line;
        self.flush_line()
    }

    fn csv_text(&self, line: &mut String, text: &str) {
        let quote = self.options.quote;
        let guard = self.options.formula_guard && needs_formula_guard(text);
        let delimiter = self.options.effective_delimiter();
        let needs_quotes = self.options.quote_style == QuoteStyle::Always
            || text.contains([delimiter, quote, '\r', '\n'])
            || (text.is_empty() && self.options.text.null_text.is_empty())
            || (!text.is_empty() && text == self.options.text.null_text);
        if needs_quotes {
            line.push(quote);
        }
        if guard {
            line.push('\'');
        }
        for c in text.chars() {
            if c == quote {
                line.push(quote);
            }
            line.push(c);
        }
        if needs_quotes {
            line.push(quote);
        }
    }

    fn csv_value(&self, line: &mut String, value: &CellValue<'_>, scratch: &mut String) {
        if matches!(value, CellValue::Null) {
            // The NULL text is always written bare, so it reads back as NULL; a text equal to
            // it is quoted (`csv_text`).
            line.push_str(&self.options.text.null_text);
            return;
        }
        scratch.clear();
        self.options.text.write(scratch, value);
        if value.is_number() {
            // Numbers: no guard; quoted only if the decimal separator is the delimiter.
            let delimiter = self.options.effective_delimiter();
            if self.options.quote_style == QuoteStyle::Always || scratch.contains(delimiter) {
                line.push(self.options.quote);
                line.push_str(scratch);
                line.push(self.options.quote);
            } else {
                line.push_str(scratch);
            }
        } else {
            self.csv_text(line, scratch);
        }
    }

    fn json_value(&self, line: &mut String, value: &CellValue<'_>, scratch: &mut String) {
        match value {
            CellValue::Null => line.push_str("null"),
            CellValue::Bool(b) => line.push_str(if *b { "true" } else { "false" }),
            CellValue::Int(i) => line.push_str(&i.to_string()),
            CellValue::UInt(u) => line.push_str(&u.to_string()),
            CellValue::Float(f) if f.is_finite() => float_text(line, *f),
            CellValue::Float(f) => {
                scratch.clear();
                float_text(scratch, *f);
                write_json_string(line, scratch);
            }
            CellValue::Decimal(d) => line.push_str(d),
            CellValue::Bytes(b) => {
                scratch.clear();
                write_hex(scratch, b);
                write_json_string(line, scratch);
            }
            other => {
                scratch.clear();
                // Dates and text keep their text form; the NULL text never applies here.
                self.options.text.write(scratch, other);
                write_json_string(line, scratch);
            }
        }
    }

    /// Writes every row of `batch`.
    ///
    /// # Errors
    /// I/O errors; SQL literal errors.
    pub fn write_batch(&mut self, batch: &RecordBatch) -> Result<(), TransferError> {
        if self.state == State::Waiting {
            self.start(&batch.schema())?;
        }
        let schema = batch.schema();
        let mut cut = false;
        for f in schema.fields() {
            if f.metadata()
                .get(TRUNCATED_VALUES_METADATA_KEY)
                .is_some_and(|v| v == "true")
            {
                cut = true;
                if !self.truncated.contains(f.name()) {
                    self.truncated.insert(f.name().clone());
                }
            }
        }
        if cut {
            self.truncated_batches += 1;
        }
        let columns = batch.columns();
        let mut scratch = String::new();
        let eol = self.eol();
        for row in 0..batch.num_rows() {
            let mut line = std::mem::take(&mut self.line);
            match self.options.format {
                ExportFormat::Csv | ExportFormat::Tsv => {
                    let delimiter = self.options.effective_delimiter();
                    for (i, col) in columns.iter().enumerate() {
                        if i > 0 {
                            line.push(delimiter);
                        }
                        self.csv_value(&mut line, &cell(col, row), &mut scratch);
                    }
                    line.push_str(eol);
                }
                ExportFormat::JsonArray | ExportFormat::JsonLines => {
                    if self.options.format == ExportFormat::JsonArray {
                        line.push_str(if self.rows == 0 { "" } else { "," });
                        line.push_str(eol);
                        line.push_str("  ");
                    }
                    line.push('{');
                    for (i, col) in columns.iter().enumerate() {
                        if i > 0 {
                            line.push_str(", ");
                        }
                        let key = self.json_keys.get(i).map_or("", String::as_str);
                        write_json_string(&mut line, key);
                        line.push_str(": ");
                        self.json_value(&mut line, &cell(col, row), &mut scratch);
                    }
                    line.push('}');
                    if self.options.format == ExportFormat::JsonLines {
                        line.push_str(eol);
                    }
                }
                ExportFormat::SqlInsert => {
                    let Some(sql) = self.sql.clone() else {
                        return Err(TransferError::InvalidOption("no SQL renderer".into()));
                    };
                    if self.rows_in_statement == 0 {
                        line.push_str(&self.sql_prefix);
                        line.push_str(if self.options.sql.rows_per_statement > 1 {
                            eol
                        } else {
                            " "
                        });
                    } else {
                        line.push(',');
                        line.push_str(eol);
                    }
                    match sql.row_values(&self.options.sql.table, batch, row) {
                        Ok(tuple) => line.push_str(&tuple),
                        Err(e) => {
                            self.line = line;
                            self.line.clear();
                            return Err(e);
                        }
                    }
                    self.rows_in_statement += 1;
                    if self.rows_in_statement >= self.options.sql.rows_per_statement {
                        line.push(';');
                        line.push_str(eol);
                        self.rows_in_statement = 0;
                    }
                }
                ExportFormat::Markdown => {
                    line.push('|');
                    for col in columns {
                        line.push(' ');
                        scratch.clear();
                        self.options.text.write(&mut scratch, &cell(col, row));
                        write_markdown_escaped(&mut line, &scratch);
                        line.push_str(" |");
                    }
                    line.push_str(eol);
                }
                ExportFormat::Html => {
                    line.push_str("<tr>");
                    for col in columns {
                        let value = cell(col, row);
                        line.push_str(if matches!(value, CellValue::Null) {
                            "<td class=\"null\">"
                        } else {
                            "<td>"
                        });
                        scratch.clear();
                        self.options.text.write(&mut scratch, &value);
                        write_html_escaped(&mut line, &scratch);
                        line.push_str("</td>");
                    }
                    line.push_str("</tr>");
                    line.push_str(eol);
                }
            }
            self.line = line;
            self.flush_line()?;
            self.rows += 1;
        }
        // Keep the row buffer from growing without bound after one huge row.
        if self.line.capacity() > 1024 * 1024 {
            self.line = String::with_capacity(1024);
        }
        Ok(())
    }

    /// Ends the document (closing brackets and tags, the last SQL statement), flushes and
    /// returns the output with the counters.
    ///
    /// # Errors
    /// I/O errors.
    pub fn finish(mut self) -> Result<(W, ExportStats), TransferError> {
        let eol = self.eol();
        if self.state == State::Waiting {
            // No result set: the prologue still makes a valid empty document.
            self.write_prologue()?;
        }
        let mut line = std::mem::take(&mut self.line);
        match self.options.format {
            ExportFormat::JsonArray => {
                if self.rows > 0 {
                    line.push_str(eol);
                }
                line.push(']');
                line.push_str(eol);
            }
            ExportFormat::SqlInsert if self.rows_in_statement > 0 => {
                line.push(';');
                line.push_str(eol);
            }
            ExportFormat::Html => {
                line.push_str("</tbody></table></body></html>");
                line.push_str(eol);
            }
            _ => {}
        }
        self.line = line;
        self.flush_line()?;
        self.out.flush()?;
        let stats = ExportStats {
            rows: self.rows,
            bytes: self.out.bytes,
            truncated_value_columns: self.truncated.into_iter().collect(),
            batches_with_truncated_values: self.truncated_batches,
            result_sets_skipped: self.skipped,
            incomplete: self.incomplete,
        };
        Ok((self.out.inner, stats))
    }
}

/// JSON keys for the column names: duplicates get `_2`, `_3`… suffixes.
fn unique_keys(columns: &[String]) -> Vec<String> {
    let mut seen: HashMap<String, usize> = HashMap::new();
    let taken: BTreeSet<&str> = columns.iter().map(String::as_str).collect();
    let mut out = Vec::with_capacity(columns.len());
    for name in columns {
        let count = seen.entry(name.clone()).or_insert(0);
        *count += 1;
        if *count == 1 {
            out.push(name.clone());
        } else {
            let mut n = *count;
            let mut key = format!("{name}_{n}");
            while taken.contains(key.as_str()) || out.contains(&key) {
                n += 1;
                key = format!("{name}_{n}");
            }
            out.push(key);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_json_keys() {
        let keys = unique_keys(&["a".into(), "a".into(), "a_2".into(), "b".into()]);
        assert_eq!(keys, vec!["a", "a_3", "a_2", "b"]);
    }

    #[test]
    fn markdown_escapes_everything_active() {
        let mut s = String::new();
        write_markdown_escaped(&mut s, "a|b *c* [x](javascript:y) <img src=x>\r\nz");
        assert_eq!(
            s,
            "a\\|b \\*c\\* \\[x\\]\\(javascript\\:y\\) \\<img src\\=x\\><br>z"
        );
    }
}
