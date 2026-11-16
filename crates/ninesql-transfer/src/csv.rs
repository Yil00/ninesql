//! A streaming CSV reader with hard limits, for the CSV import (F-XFER-03).
//!
//! RFC 4180 with the usual leniencies: `\n`, `\r\n` or a lone `\r` end a record; a quoted field
//! may span lines and holds `""` for a quote; a UTF-8 byte order mark at the start is skipped.
//! The reader keeps one record in memory and refuses inputs that would grow it without bound:
//! - a record longer than [`CsvLimits::max_record_bytes`] is a fatal error (inside quotes the
//!   reader cannot find where the next record starts), so is a quote still open at the end;
//! - a record with more than [`CsvLimits::max_fields`] fields, a field that is not valid UTF-8 or
//!   a character after a closing quote is a **record** error ([`RecordError`]): the record is
//!   skipped and reading goes on.
//!
//! Errors carry the line and a reason, never the content (a cell may be a secret, I1).

use std::io::BufRead;

use crate::error::TransferError;

/// Hard limits of the reader and the import.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct CsvLimits {
    /// Largest file accepted, in bytes (default 8 GiB).
    pub max_file_bytes: u64,
    /// Longest record, in bytes, line breaks inside quotes included (default 1 MiB).
    pub max_record_bytes: usize,
    /// Most fields in one record (default 4 096).
    pub max_fields: usize,
}

impl Default for CsvLimits {
    fn default() -> Self {
        Self {
            max_file_bytes: 8 * 1024 * 1024 * 1024,
            max_record_bytes: 1024 * 1024,
            max_fields: 4096,
        }
    }
}

impl CsvLimits {
    /// Sets `max_file_bytes`.
    #[must_use]
    pub fn with_max_file_bytes(mut self, bytes: u64) -> Self {
        self.max_file_bytes = bytes;
        self
    }

    /// Sets `max_record_bytes`.
    #[must_use]
    pub fn with_max_record_bytes(mut self, bytes: usize) -> Self {
        self.max_record_bytes = bytes;
        self
    }

    /// Sets `max_fields`.
    #[must_use]
    pub fn with_max_fields(mut self, fields: usize) -> Self {
        self.max_fields = fields;
        self
    }
}

/// How the file is read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct CsvFormat {
    /// Field delimiter (one ASCII byte, default `,`).
    pub delimiter: u8,
    /// Quote (one ASCII byte, default `"`), or `None` to read quotes as data.
    pub quote: Option<u8>,
    /// The first record is a header (default `true`).
    pub header: bool,
}

impl Default for CsvFormat {
    fn default() -> Self {
        Self {
            delimiter: b',',
            quote: Some(b'"'),
            header: true,
        }
    }
}

impl CsvFormat {
    /// Checks the format.
    ///
    /// # Errors
    /// [`TransferError::InvalidOption`] for a non-ASCII delimiter or quote, a line break, or a
    /// delimiter equal to the quote.
    pub fn validate(&self) -> Result<(), TransferError> {
        let bad = |b: u8| !b.is_ascii() || b == b'\r' || b == b'\n';
        if bad(self.delimiter) || self.quote.is_some_and(bad) || self.quote == Some(self.delimiter)
        {
            return Err(TransferError::InvalidOption(
                "the delimiter and the quote must be distinct ASCII characters other than line breaks"
                    .into(),
            ));
        }
        Ok(())
    }
}

/// Why one record was skipped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RecordErrorKind {
    /// More fields than [`CsvLimits::max_fields`].
    TooManyFields,
    /// A field is not valid UTF-8.
    InvalidUtf8,
    /// A character follows a closing quote.
    TextAfterQuote,
}

impl RecordErrorKind {
    /// Short description.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TooManyFields => "too many fields",
            Self::InvalidUtf8 => "a field is not valid UTF-8",
            Self::TextAfterQuote => "text after a closing quote",
        }
    }
}

/// A record that could not be read; the reader goes on with the next one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecordError {
    /// 1-based line where the record starts.
    pub line: u64,
    /// What is wrong.
    pub kind: RecordErrorKind,
}

/// One record. `Debug` shows the line and the field count only.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct Record {
    /// 1-based line where the record starts.
    pub line: u64,
    /// Fields.
    pub fields: Vec<String>,
}

impl std::fmt::Debug for Record {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Record")
            .field("line", &self.line)
            .field("fields", &self.fields.len())
            .finish()
    }
}

/// The streaming reader.
pub struct CsvReader<R> {
    input: R,
    format: CsvFormat,
    limits: CsvLimits,
    line: u64,
    bytes_read: u64,
    started: bool,
    eof: bool,
    /// A lone `\r` ended the previous record: skip a `\n` right after it.
    skip_lf: bool,
}

impl<R> std::fmt::Debug for CsvReader<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CsvReader")
            .field("line", &self.line)
            .field("bytes_read", &self.bytes_read)
            .finish_non_exhaustive()
    }
}

enum FieldState {
    Start,
    Unquoted,
    Quoted,
    /// A quote inside a quoted field: either the end or the first half of `""`.
    QuoteInQuoted,
    /// After the closing quote.
    AfterQuote,
}

impl<R: BufRead> CsvReader<R> {
    /// A reader over `input`.
    ///
    /// # Errors
    /// [`CsvFormat::validate`].
    pub fn new(input: R, format: CsvFormat, limits: CsvLimits) -> Result<Self, TransferError> {
        format.validate()?;
        Ok(Self {
            input,
            format,
            limits,
            line: 1,
            bytes_read: 0,
            started: false,
            eof: false,
            skip_lf: false,
        })
    }

    /// Bytes consumed so far.
    #[must_use]
    pub fn bytes_read(&self) -> u64 {
        self.bytes_read
    }

    fn next_byte(&mut self) -> Result<Option<u8>, TransferError> {
        loop {
            let buf = self.input.fill_buf()?;
            let Some(&b) = buf.first() else {
                return Ok(None);
            };
            self.input.consume(1);
            self.bytes_read += 1;
            if self.bytes_read > self.limits.max_file_bytes {
                return Err(TransferError::FileTooLarge {
                    limit: self.limits.max_file_bytes,
                });
            }
            if self.skip_lf {
                self.skip_lf = false;
                if b == b'\n' {
                    continue;
                }
            }
            return Ok(Some(b));
        }
    }

    fn skip_bom(&mut self) -> Result<(), TransferError> {
        let buf = self.input.fill_buf()?;
        if buf.starts_with(&[0xEF, 0xBB, 0xBF]) {
            self.input.consume(3);
            self.bytes_read += 3;
        }
        Ok(())
    }

    /// The next record: `Ok(None)` at the end, `Ok(Some(Err(_)))` for a record skipped with its
    /// reason.
    ///
    /// # Errors
    /// Fatal errors: I/O, a record over the size cap, a quote still open at the end of the
    /// input, a file over the size cap.
    #[allow(clippy::too_many_lines)] // One state machine.
    pub fn next_record(&mut self) -> Result<Option<Result<Record, RecordError>>, TransferError> {
        if self.eof {
            return Ok(None);
        }
        if !self.started {
            self.started = true;
            self.skip_bom()?;
        }
        let start_line = self.line;
        let mut fields: Vec<String> = Vec::new();
        let mut field: Vec<u8> = Vec::new();
        let mut state = FieldState::Start;
        let mut record_bytes = 0usize;
        let mut error: Option<RecordErrorKind> = None;
        let mut any = false;
        let delimiter = self.format.delimiter;
        let quote = self.format.quote;
        let max_fields = self.limits.max_fields;

        let push_field =
            |fields: &mut Vec<String>, field: &mut Vec<u8>, error: &mut Option<RecordErrorKind>| {
                if fields.len() >= max_fields {
                    error.get_or_insert(RecordErrorKind::TooManyFields);
                    field.clear();
                    return;
                }
                match String::from_utf8(std::mem::take(field)) {
                    Ok(s) => fields.push(s),
                    Err(_) => {
                        error.get_or_insert(RecordErrorKind::InvalidUtf8);
                        fields.push(String::new());
                    }
                }
            };

        loop {
            let Some(b) = self.next_byte()? else {
                self.eof = true;
                if matches!(state, FieldState::Quoted) {
                    return Err(TransferError::Csv {
                        line: start_line,
                        reason: "a quoted field is not closed at the end of the file".into(),
                    });
                }
                if !any {
                    return Ok(None);
                }
                push_field(&mut fields, &mut field, &mut error);
                break;
            };
            any = true;
            record_bytes += 1;
            if record_bytes > self.limits.max_record_bytes {
                return Err(TransferError::Csv {
                    line: start_line,
                    reason: format!(
                        "a record is longer than the limit of {} bytes",
                        self.limits.max_record_bytes
                    ),
                });
            }
            let is_eol = b == b'\n' || b == b'\r';
            match state {
                FieldState::Start | FieldState::Unquoted | FieldState::AfterQuote
                    if is_eol || b == delimiter =>
                {
                    push_field(&mut fields, &mut field, &mut error);
                    if is_eol {
                        self.line += 1;
                        if b == b'\r' {
                            self.skip_lf = true;
                        }
                        break;
                    }
                    state = FieldState::Start;
                }
                FieldState::Start if Some(b) == quote => state = FieldState::Quoted,
                FieldState::Start | FieldState::Unquoted => {
                    field.push(b);
                    state = FieldState::Unquoted;
                }
                FieldState::AfterQuote => {
                    error.get_or_insert(RecordErrorKind::TextAfterQuote);
                    field.push(b);
                }
                FieldState::Quoted if Some(b) == quote => state = FieldState::QuoteInQuoted,
                FieldState::Quoted => {
                    if b == b'\n' {
                        self.line += 1;
                    }
                    field.push(b);
                }
                FieldState::QuoteInQuoted if Some(b) == quote => {
                    field.push(b);
                    state = FieldState::Quoted;
                }
                FieldState::QuoteInQuoted => {
                    if is_eol || b == delimiter {
                        push_field(&mut fields, &mut field, &mut error);
                        if is_eol {
                            self.line += 1;
                            if b == b'\r' {
                                self.skip_lf = true;
                            }
                            break;
                        }
                        state = FieldState::Start;
                    } else {
                        error.get_or_insert(RecordErrorKind::TextAfterQuote);
                        field.push(b);
                        state = FieldState::AfterQuote;
                    }
                }
            }
        }
        Ok(Some(match error {
            Some(kind) => Err(RecordError {
                line: start_line,
                kind,
            }),
            None => Ok(Record {
                line: start_line,
                fields,
            }),
        }))
    }

    /// Like [`Self::next_record`], skipping blank lines (a record with one empty field).
    ///
    /// # Errors
    /// See [`Self::next_record`].
    pub fn next_non_blank(&mut self) -> Result<Option<Result<Record, RecordError>>, TransferError> {
        loop {
            match self.next_record()? {
                Some(Ok(r)) if r.fields.len() == 1 && r.fields[0].is_empty() => {}
                other => return Ok(other),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_all(
        input: &str,
        limits: CsvLimits,
    ) -> Result<Vec<Result<Record, RecordError>>, TransferError> {
        let mut r = CsvReader::new(input.as_bytes(), CsvFormat::default(), limits)?;
        let mut out = Vec::new();
        while let Some(rec) = r.next_record()? {
            out.push(rec);
        }
        Ok(out)
    }

    fn fields(rec: &Result<Record, RecordError>) -> Vec<&str> {
        rec.as_ref()
            .unwrap()
            .fields
            .iter()
            .map(String::as_str)
            .collect()
    }

    #[test]
    fn rfc4180() {
        let recs = read_all(
            "\u{FEFF}a,b,c\r\n1,\"x, \"\"y\"\"\",\r\n\"multi\nline\",2,3\n4,5,6",
            CsvLimits::default(),
        )
        .unwrap();
        assert_eq!(recs.len(), 4);
        assert_eq!(fields(&recs[0]), ["a", "b", "c"]);
        assert_eq!(fields(&recs[1]), ["1", "x, \"y\"", ""]);
        assert_eq!(fields(&recs[2]), ["multi\nline", "2", "3"]);
        assert_eq!(recs[2].as_ref().unwrap().line, 3);
        assert_eq!(recs[3].as_ref().unwrap().line, 5);
    }

    #[test]
    fn lone_cr_and_trailing_newline() {
        let recs = read_all("a\rb\r\n\nc\n", CsvLimits::default()).unwrap();
        let all: Vec<Vec<&str>> = recs.iter().map(fields).collect();
        assert_eq!(all, vec![vec!["a"], vec!["b"], vec![""], vec!["c"]]);
    }

    #[test]
    fn record_errors_do_not_stop_reading() {
        let recs = read_all(
            "\"a\"x,b\n1,2,3,4\nok,1\n",
            CsvLimits::default().with_max_fields(3),
        )
        .unwrap();
        assert_eq!(
            recs[0].as_ref().unwrap_err().kind,
            RecordErrorKind::TextAfterQuote
        );
        assert_eq!(
            recs[1].as_ref().unwrap_err().kind,
            RecordErrorKind::TooManyFields
        );
        assert_eq!(fields(&recs[2]), ["ok", "1"]);
        let mut r =
            CsvReader::new(&b"\xff,1\n"[..], CsvFormat::default(), CsvLimits::default()).unwrap();
        assert_eq!(
            r.next_record().unwrap().unwrap().unwrap_err().kind,
            RecordErrorKind::InvalidUtf8
        );
    }

    #[test]
    fn fatal_errors() {
        assert!(matches!(
            read_all("a,\"open\n", CsvLimits::default()),
            Err(TransferError::Csv { line: 1, .. })
        ));
        assert!(matches!(
            read_all(
                "0123456789\n",
                CsvLimits::default().with_max_record_bytes(5)
            ),
            Err(TransferError::Csv { .. })
        ));
        assert!(matches!(
            read_all("0123456789\n", CsvLimits::default().with_max_file_bytes(5)),
            Err(TransferError::FileTooLarge { .. })
        ));
    }

    proptest::proptest! {
        #[test]
        fn never_panics_and_respects_limits(input in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..400)) {
            let limits = CsvLimits::default().with_max_record_bytes(64).with_max_fields(8);
            if let Ok(mut r) = CsvReader::new(&input[..], CsvFormat::default(), limits) {
                while let Ok(Some(rec)) = r.next_record() {
                    if let Ok(rec) = rec {
                        proptest::prop_assert!(rec.fields.len() <= 8);
                    }
                }
            }
        }
    }
}
