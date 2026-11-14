//! MySQL / MariaDB column types → Arrow, and batch building under the fetch budget (ADR-0004,
//! I6). Values arrive in the text protocol and are parsed here.
//!
//! | MySQL | Arrow |
//! |---|---|
//! | `TINYINT` / `SMALLINT` / `MEDIUMINT`, `INT` / `BIGINT` | `Int8` / `Int16` / `Int32` / `Int64` (`UInt*` when `UNSIGNED`) |
//! | `YEAR` | `Int16` (`ninesql.type` = `year`) |
//! | `FLOAT` / `DOUBLE` | `Float32` / `Float64` |
//! | `DATE` | `Date32` (zero or invalid dates → NULL, column flagged) |
//! | `DATETIME` | `Timestamp(µs)` (zero dates → NULL, flagged) |
//! | `TIMESTAMP` | `Timestamp(µs)` in the session time zone (`ninesql.type` = `timestamp`) |
//! | `CHAR`, `VARCHAR`, `TEXT` types | `Utf8` |
//! | `BINARY`, `VARBINARY`, `BLOB` types | `Binary` |
//! | `DECIMAL`, `TIME` (± 838 h), `JSON`, `ENUM`, `SET` | `Utf8` with `ninesql.type` |
//! | `BIT`, `GEOMETRY`, `VECTOR` | `Binary` with `ninesql.type` |
//!
//! `DECIMAL` is text because precision 65 has no Arrow decimal shared by every reader; `TIME`
//! is text because it is a duration up to ± 838:59:59, not a time of day.

use std::sync::Arc;

use arrow_array::builder::{
    ArrayBuilder, BinaryBuilder, Date32Builder, Float32Builder, Float64Builder, Int8Builder,
    Int16Builder, Int32Builder, Int64Builder, StringBuilder, TimestampMicrosecondBuilder,
    UInt8Builder, UInt16Builder, UInt32Builder, UInt64Builder,
};
use arrow_array::{ArrayRef, RecordBatch};
use arrow_schema::{DataType, Field, Schema, SchemaRef, TimeUnit};
use ninesql_driver_api::{
    BatchLimiter, DriverError, DriverResult, ENGINE_TYPE_METADATA_KEY, FetchBudget,
    TRUNCATED_VALUES_METADATA_KEY,
};

use crate::proto::Buf;

const FLAG_UNSIGNED: u16 = 32;
const FLAG_ENUM: u16 = 256;
const FLAG_SET: u16 = 2048;
const BINARY_CHARSET: u16 = 63;

/// A column definition (protocol 4.1).
#[derive(Clone, Debug)]
pub(crate) struct Column {
    pub(crate) name: String,
    pub(crate) type_code: u8,
    pub(crate) flags: u16,
    pub(crate) charset: u16,
}

impl Column {
    pub(crate) fn parse(data: &[u8]) -> DriverResult<Self> {
        let mut b = Buf::new(data);
        for _ in 0..4 {
            b.lenenc_bytes()?; // catalog, schema, table, org_table
        }
        let name = String::from_utf8_lossy(b.lenenc_bytes()?).into_owned();
        b.lenenc_bytes()?; // org_name
        b.lenenc()?; // length of the fixed fields
        let charset = b.u16()?;
        let _length = b.u32()?;
        let type_code = b.u8()?;
        let flags = b.u16()?;
        Ok(Self {
            name,
            type_code,
            flags,
            charset,
        })
    }

    fn unsigned(&self) -> bool {
        self.flags & FLAG_UNSIGNED != 0
    }
}

/// Decoding of one column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    I8,
    I16,
    I32,
    I64,
    U8,
    U16,
    U32,
    U64,
    F32,
    F64,
    Date,
    Timestamp,
    Binary,
    Text,
}

impl Kind {
    fn data_type(self) -> DataType {
        match self {
            Self::I8 => DataType::Int8,
            Self::I16 => DataType::Int16,
            Self::I32 => DataType::Int32,
            Self::I64 => DataType::Int64,
            Self::U8 => DataType::UInt8,
            Self::U16 => DataType::UInt16,
            Self::U32 => DataType::UInt32,
            Self::U64 => DataType::UInt64,
            Self::F32 => DataType::Float32,
            Self::F64 => DataType::Float64,
            Self::Date => DataType::Date32,
            Self::Timestamp => DataType::Timestamp(TimeUnit::Microsecond, None),
            Self::Binary => DataType::Binary,
            Self::Text => DataType::Utf8,
        }
    }

    /// Parsed from a short text form (numbers, dates) rather than kept as bytes.
    pub(crate) fn is_parsed(self) -> bool {
        !matches!(self, Self::Binary | Self::Text)
    }
}

/// How a column is decoded and the engine type name recorded when Arrow is not faithful.
pub(crate) fn plan(c: &Column) -> (Kind, Option<&'static str>) {
    let u = c.unsigned();
    let binary = c.charset == BINARY_CHARSET;
    match c.type_code {
        1 => (if u { Kind::U8 } else { Kind::I8 }, None),
        2 => (if u { Kind::U16 } else { Kind::I16 }, None),
        9 | 3 => (if u { Kind::U32 } else { Kind::I32 }, None),
        8 => (if u { Kind::U64 } else { Kind::I64 }, None),
        13 => (Kind::I16, Some("year")),
        4 => (Kind::F32, None),
        5 => (Kind::F64, None),
        10 | 14 => (Kind::Date, None),
        12 | 18 => (Kind::Timestamp, None),
        7 | 17 => (Kind::Timestamp, Some("timestamp")),
        0 | 246 => (Kind::Text, Some("decimal")),
        11 | 19 => (Kind::Text, Some("time")),
        245 => (Kind::Text, Some("json")),
        247 => (Kind::Text, Some("enum")),
        248 => (Kind::Text, Some("set")),
        16 => (Kind::Binary, Some("bit")),
        255 => (Kind::Binary, Some("geometry")),
        242 => (Kind::Binary, Some("vector")),
        6 => (Kind::Text, Some("null")),
        15 | 253 | 254 | 249..=252 if c.flags & FLAG_ENUM != 0 => (Kind::Text, Some("enum")),
        15 | 253 | 254 | 249..=252 if c.flags & FLAG_SET != 0 => (Kind::Text, Some("set")),
        15 | 253 | 254 | 249..=252 if binary => (Kind::Binary, None),
        15 | 253 | 254 | 249..=252 => (Kind::Text, None),
        _ => (Kind::Text, Some("unknown")),
    }
}

/// The Arrow schema of a result (fields nullable).
pub(crate) fn schema_for(cols: &[Column]) -> (SchemaRef, Vec<Kind>) {
    let mut kinds = Vec::with_capacity(cols.len());
    let fields: Vec<Field> = cols
        .iter()
        .map(|c| {
            let (kind, type_name) = plan(c);
            kinds.push(kind);
            let f = Field::new(c.name.clone(), kind.data_type(), true);
            match type_name {
                Some(t) => f.with_metadata([(ENGINE_TYPE_METADATA_KEY.to_owned(), t.to_owned())]),
                None => f,
            }
        })
        .collect();
    (Arc::new(Schema::new(fields)), kinds)
}

enum ColBuilder {
    I8(Int8Builder),
    I16(Int16Builder),
    I32(Int32Builder),
    I64(Int64Builder),
    U8(UInt8Builder),
    U16(UInt16Builder),
    U32(UInt32Builder),
    U64(UInt64Builder),
    F32(Float32Builder),
    F64(Float64Builder),
    Date(Date32Builder),
    Ts(TimestampMicrosecondBuilder),
    Binary(BinaryBuilder),
    Utf8(StringBuilder),
}

macro_rules! each {
    ($self:expr, $b:ident => $e:expr) => {
        match $self {
            ColBuilder::I8($b) => $e,
            ColBuilder::I16($b) => $e,
            ColBuilder::I32($b) => $e,
            ColBuilder::I64($b) => $e,
            ColBuilder::U8($b) => $e,
            ColBuilder::U16($b) => $e,
            ColBuilder::U32($b) => $e,
            ColBuilder::U64($b) => $e,
            ColBuilder::F32($b) => $e,
            ColBuilder::F64($b) => $e,
            ColBuilder::Date($b) => $e,
            ColBuilder::Ts($b) => $e,
            ColBuilder::Binary($b) => $e,
            ColBuilder::Utf8($b) => $e,
        }
    };
}

impl ColBuilder {
    fn new(kind: Kind) -> Self {
        match kind {
            Kind::I8 => Self::I8(Int8Builder::new()),
            Kind::I16 => Self::I16(Int16Builder::new()),
            Kind::I32 => Self::I32(Int32Builder::new()),
            Kind::I64 => Self::I64(Int64Builder::new()),
            Kind::U8 => Self::U8(UInt8Builder::new()),
            Kind::U16 => Self::U16(UInt16Builder::new()),
            Kind::U32 => Self::U32(UInt32Builder::new()),
            Kind::U64 => Self::U64(UInt64Builder::new()),
            Kind::F32 => Self::F32(Float32Builder::new()),
            Kind::F64 => Self::F64(Float64Builder::new()),
            Kind::Date => Self::Date(Date32Builder::new()),
            Kind::Timestamp => Self::Ts(TimestampMicrosecondBuilder::new()),
            Kind::Binary => Self::Binary(BinaryBuilder::new()),
            Kind::Text => Self::Utf8(StringBuilder::new()),
        }
    }

    fn append_null(&mut self) {
        each!(self, b => b.append_null());
    }

    fn finish(&mut self) -> ArrayRef {
        each!(self, b => Arc::new(b.finish()) as ArrayRef)
    }

    fn len(&self) -> usize {
        each!(self, b => b.len())
    }
}

fn bad(what: &str) -> DriverError {
    DriverError::Protocol(format!("invalid {what} value from the server"))
}

fn text(v: &[u8]) -> DriverResult<&str> {
    std::str::from_utf8(v).map_err(|_| bad("text"))
}

fn num<T: std::str::FromStr>(v: &[u8]) -> DriverResult<T> {
    text(v)?.trim().parse().map_err(|_| bad("numeric"))
}

/// Days since 1970-01-01 of a civil date (proleptic Gregorian, H. Hinnant's algorithm).
pub(crate) fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = i64::from(m);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `YYYY-MM-DD` → days, `None` for zero or invalid dates (`0000-00-00`, `2026-02-30`).
pub(crate) fn parse_date(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let y: i64 = s[..4].parse().ok()?;
    let m: u32 = s[5..7].parse().ok()?;
    let d: u32 = s[8..10].parse().ok()?;
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let dim = match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return None,
    };
    if y == 0 || d == 0 || d > dim {
        return None;
    }
    Some(days_from_civil(y, m, d))
}

/// `YYYY-MM-DD hh:mm:ss[.ffffff]` → microseconds since the epoch, `None` for zero dates.
pub(crate) fn parse_datetime(s: &str) -> Option<i64> {
    let days = parse_date(s.get(..10)?)?;
    let rest = s.get(10..)?;
    let rest = rest.strip_prefix(' ').unwrap_or(rest);
    if rest.is_empty() {
        return days.checked_mul(86_400_000_000);
    }
    let (hms, frac) = match rest.split_once('.') {
        Some((a, f)) => (a, f),
        None => (rest, ""),
    };
    let mut it = hms.split(':');
    let h: i64 = it.next()?.parse().ok()?;
    let mi: i64 = it.next()?.parse().ok()?;
    let se: i64 = it.next()?.parse().ok()?;
    if it.next().is_some() || h > 23 || mi > 59 || se > 59 || frac.len() > 6 {
        return None;
    }
    let micros: i64 = if frac.is_empty() {
        0
    } else {
        format!("{frac:0<6}").parse().ok()?
    };
    days.checked_mul(86_400_000_000)?
        .checked_add((h * 3600 + mi * 60 + se) * 1_000_000 + micros)
}

/// Builds the batches of one result set. Values are appended by the connection's row reader,
/// which already cut variable-length values to the row budget.
pub(crate) struct BatchBuilder {
    schema: SchemaRef,
    kinds: Vec<Kind>,
    cols: Vec<ColBuilder>,
    truncated: Vec<bool>,
    budget: FetchBudget,
    limiter: BatchLimiter,
    /// Columns where a zero / invalid date was replaced by NULL.
    zero_dates: Vec<bool>,
    zero_reported: Vec<bool>,
}

impl BatchBuilder {
    pub(crate) fn new(schema: SchemaRef, kinds: Vec<Kind>, budget: FetchBudget) -> Self {
        let cols = kinds.iter().map(|k| ColBuilder::new(*k)).collect();
        let n = kinds.len();
        Self {
            schema,
            kinds,
            cols,
            truncated: vec![false; n],
            limiter: BatchLimiter::new(&budget),
            budget,
            zero_dates: vec![false; n],
            zero_reported: vec![false; n],
        }
    }

    pub(crate) fn columns(&self) -> usize {
        self.kinds.len()
    }

    pub(crate) fn budget(&self) -> &FetchBudget {
        &self.budget
    }

    pub(crate) fn is_full(&self) -> bool {
        self.limiter.is_full()
    }

    pub(crate) fn is_parsed(&self, col: usize) -> bool {
        self.kinds[col].is_parsed()
    }

    pub(crate) fn null(&mut self, col: usize) {
        self.cols[col].append_null();
    }

    /// A short text value of a parsed kind.
    pub(crate) fn parsed(&mut self, col: usize, v: &[u8]) -> DriverResult<()> {
        match &mut self.cols[col] {
            ColBuilder::I8(b) => b.append_value(num(v)?),
            ColBuilder::I16(b) => b.append_value(num(v)?),
            ColBuilder::I32(b) => b.append_value(num(v)?),
            ColBuilder::I64(b) => b.append_value(num(v)?),
            ColBuilder::U8(b) => b.append_value(num(v)?),
            ColBuilder::U16(b) => b.append_value(num(v)?),
            ColBuilder::U32(b) => b.append_value(num(v)?),
            ColBuilder::U64(b) => b.append_value(num(v)?),
            ColBuilder::F32(b) => b.append_value(num(v)?),
            ColBuilder::F64(b) => b.append_value(num(v)?),
            ColBuilder::Date(b) => match parse_date(text(v)?) {
                Some(d) => b.append_value(i32::try_from(d).map_err(|_| bad("date"))?),
                None => {
                    b.append_null();
                    self.zero_dates[col] = true;
                    self.truncated[col] = true;
                }
            },
            ColBuilder::Ts(b) => match parse_datetime(text(v)?) {
                Some(t) => b.append_value(t),
                None => {
                    b.append_null();
                    self.zero_dates[col] = true;
                    self.truncated[col] = true;
                }
            },
            ColBuilder::Binary(_) | ColBuilder::Utf8(_) => return Err(bad("column")),
        }
        Ok(())
    }

    /// A variable-length value, already cut (`cut` = it was).
    pub(crate) fn bytes(&mut self, col: usize, v: Vec<u8>, cut: bool) {
        if cut {
            self.truncated[col] = true;
        }
        match &mut self.cols[col] {
            ColBuilder::Binary(b) => b.append_value(&v),
            ColBuilder::Utf8(b) => {
                let s = crate::conn::utf8_prefix(v);
                b.append_value(s);
            }
            other => other.append_null(),
        }
    }

    pub(crate) fn end_row(&mut self, bytes: u64) {
        self.limiter.add_row(bytes);
    }

    /// Names of columns that got their first zero date since the last call.
    pub(crate) fn take_new_zero_date_columns(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        for i in 0..self.kinds.len() {
            if self.zero_dates[i] && !self.zero_reported[i] {
                self.zero_reported[i] = true;
                out.push(self.schema.field(i).name().clone());
            }
        }
        out
    }

    /// Emits the current batch (`None` when empty). Columns holding cut values are flagged
    /// with `ninesql.truncated` (the batch schema then differs from the header only by field
    /// metadata, which the stream allows).
    pub(crate) fn finish(&mut self) -> DriverResult<Option<RecordBatch>> {
        if self.cols.first().is_none_or(|c| c.len() == 0) {
            self.limiter.reset();
            return Ok(None);
        }
        let arrays: Vec<ArrayRef> = self.cols.iter_mut().map(ColBuilder::finish).collect();
        let fields: Vec<Field> = self
            .schema
            .fields()
            .iter()
            .zip(&self.truncated)
            .map(|(f, t)| {
                let f = f.as_ref().clone();
                if *t {
                    let mut md = f.metadata().clone();
                    md.insert(TRUNCATED_VALUES_METADATA_KEY.to_owned(), "true".to_owned());
                    f.with_metadata(md)
                } else {
                    f
                }
            })
            .collect();
        self.limiter.reset();
        let batch = RecordBatch::try_new(Arc::new(Schema::new(fields)), arrays)
            .map_err(|e| DriverError::Protocol(format!("cannot build a batch: {e}")))?;
        Ok(Some(batch))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_parse() {
        assert_eq!(parse_date("1970-01-01"), Some(0));
        assert_eq!(parse_date("2022-01-08"), Some(19_000));
        assert_eq!(parse_date("0000-00-00"), None);
        assert_eq!(parse_date("2026-02-30"), None);
        assert_eq!(parse_date("2024-02-29"), Some(19_782));
        assert_eq!(parse_date("1000-01-01"), Some(-354_285));
        assert_eq!(parse_datetime("1970-01-01 00:00:01"), Some(1_000_000));
        assert_eq!(
            parse_datetime("1970-01-01 00:00:00.5"),
            Some(500_000),
            "fraction padded to microseconds"
        );
        assert_eq!(parse_datetime("0000-00-00 00:00:00"), None);
        assert_eq!(parse_datetime("2026-11-14 25:00:00"), None);
    }

    fn col(type_code: u8, flags: u16, charset: u16) -> Column {
        Column {
            name: "c".into(),
            type_code,
            flags,
            charset,
        }
    }

    #[test]
    fn plans() {
        assert_eq!(plan(&col(8, FLAG_UNSIGNED, 63)).0, Kind::U64);
        assert_eq!(plan(&col(253, 0, 45)), (Kind::Text, None));
        assert_eq!(plan(&col(252, 0, 63)), (Kind::Binary, None));
        assert_eq!(plan(&col(254, FLAG_ENUM, 45)), (Kind::Text, Some("enum")));
        assert_eq!(plan(&col(246, 0, 63)), (Kind::Text, Some("decimal")));
        assert_eq!(plan(&col(11, 0, 63)), (Kind::Text, Some("time")));
    }

    #[test]
    fn zero_dates_are_null_and_flagged() {
        let cols = vec![col(10, 0, 63)];
        let (schema, kinds) = schema_for(&cols);
        let mut b = BatchBuilder::new(schema, kinds, FetchBudget::default());
        b.parsed(0, b"0000-00-00").unwrap();
        b.end_row(8);
        assert_eq!(b.take_new_zero_date_columns(), vec!["c".to_owned()]);
        let batch = b.finish().unwrap().unwrap();
        assert!(arrow_array::Array::is_null(batch.column(0), 0));
        assert_eq!(
            batch
                .schema()
                .field(0)
                .metadata()
                .get(TRUNCATED_VALUES_METADATA_KEY),
            Some(&"true".to_owned())
        );
        assert!(b.parsed(0, b"\xff").is_err());
    }
}
