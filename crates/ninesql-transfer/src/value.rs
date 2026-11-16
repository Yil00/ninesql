//! Typed access to Arrow values and their text forms, shared by every export format.
//!
//! [`cell`] reads one value of a column as a [`CellValue`] (borrowed text and bytes, no copy);
//! [`TextOptions`] turns it into text: the NULL text, date / time / timestamp patterns and the
//! decimal separator. Nothing here logs a value (AGENTS.md).

use std::fmt::Write as _;

use ninesql_driver_api::arrow_array::{
    Array, ArrayRef, BinaryArray, BinaryViewArray, BooleanArray, Date32Array, Date64Array,
    Decimal128Array, Decimal256Array, DurationMicrosecondArray, DurationMillisecondArray,
    DurationNanosecondArray, DurationSecondArray, FixedSizeBinaryArray, Float16Array, Float32Array,
    Float64Array, Int8Array, Int16Array, Int32Array, Int64Array, LargeBinaryArray,
    LargeStringArray, StringArray, StringViewArray, Time32MillisecondArray, Time32SecondArray,
    Time64MicrosecondArray, Time64NanosecondArray, TimestampMicrosecondArray,
    TimestampMillisecondArray, TimestampNanosecondArray, TimestampSecondArray, UInt8Array,
    UInt16Array, UInt32Array, UInt64Array,
};
use ninesql_driver_api::arrow_schema::{DataType, TimeUnit};

use crate::error::TransferError;

/// One value of a result, borrowed from its Arrow array.
#[derive(Clone, PartialEq)]
#[non_exhaustive]
pub enum CellValue<'a> {
    /// SQL `NULL`.
    Null,
    /// Boolean.
    Bool(bool),
    /// Signed integer.
    Int(i64),
    /// Unsigned integer.
    UInt(u64),
    /// Floating point.
    Float(f64),
    /// Exact decimal in canonical text form (`-12.50`).
    Decimal(String),
    /// Text.
    Text(&'a str),
    /// Binary.
    Bytes(&'a [u8]),
    /// Date, days since 1970-01-01.
    Date(i64),
    /// Time of day.
    Time {
        /// Seconds since midnight.
        secs: i64,
        /// Nanoseconds within the second.
        nanos: u32,
    },
    /// Timestamp.
    Timestamp {
        /// Seconds since 1970-01-01T00:00:00.
        secs: i64,
        /// Nanoseconds within the second.
        nanos: u32,
        /// The column has a time zone: the instant is in UTC.
        utc: bool,
    },
    /// A duration, already in text form (`3 days 01:02:03`).
    Duration(String),
    /// An Arrow type the exporter does not render (lists, structs…): its type label. Exported
    /// as `<List>`, never as a fake value (I8).
    Unsupported(String),
}

impl std::fmt::Debug for CellValue<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never prints a value.
        let kind = match self {
            Self::Null => "Null",
            Self::Bool(_) => "Bool",
            Self::Int(_) => "Int",
            Self::UInt(_) => "UInt",
            Self::Float(_) => "Float",
            Self::Decimal(_) => "Decimal",
            Self::Text(_) => "Text",
            Self::Bytes(_) => "Bytes",
            Self::Date(_) => "Date",
            Self::Time { .. } => "Time",
            Self::Timestamp { .. } => "Timestamp",
            Self::Duration(_) => "Duration",
            Self::Unsupported(_) => "Unsupported",
        };
        write!(f, "CellValue::{kind}(<redacted>)")
    }
}

impl CellValue<'_> {
    /// `true` for numbers (integers, floats, decimals).
    #[must_use]
    pub fn is_number(&self) -> bool {
        matches!(
            self,
            Self::Int(_) | Self::UInt(_) | Self::Float(_) | Self::Decimal(_)
        )
    }
}

fn unit_per_second(unit: TimeUnit) -> i64 {
    match unit {
        TimeUnit::Second => 1,
        TimeUnit::Millisecond => 1_000,
        TimeUnit::Microsecond => 1_000_000,
        TimeUnit::Nanosecond => 1_000_000_000,
    }
}

fn split_units(value: i64, unit: TimeUnit) -> (i64, u32) {
    let per = unit_per_second(unit);
    let secs = value.div_euclid(per);
    let frac = value.rem_euclid(per);
    let nanos = frac * (1_000_000_000 / per);
    (secs, u32::try_from(nanos).unwrap_or(0))
}

fn decimal_text(digits: &str, scale: i8) -> String {
    let mut out = String::with_capacity(digits.len() + 2);
    let (neg, digits) = match digits.strip_prefix('-') {
        Some(d) => (true, d),
        None => (false, digits),
    };
    if neg {
        out.push('-');
    }
    if scale <= 0 {
        out.push_str(digits);
        if digits != "0" {
            for _ in 0..scale.unsigned_abs() {
                out.push('0');
            }
        }
        return out;
    }
    let scale = usize::from(scale.unsigned_abs());
    if digits.len() <= scale {
        out.push_str("0.");
        for _ in 0..scale - digits.len() {
            out.push('0');
        }
        out.push_str(digits);
    } else {
        let (int, frac) = digits.split_at(digits.len() - scale);
        out.push_str(int);
        out.push('.');
        out.push_str(frac);
    }
    out
}

fn duration_text(value: i64, unit: TimeUnit) -> String {
    let per = i128::from(unit_per_second(unit));
    let v = i128::from(value);
    let mut out = String::new();
    if v < 0 {
        out.push('-');
    }
    let v = v.abs();
    let secs = v / per;
    let frac = v % per;
    let days = secs / 86_400;
    if days > 0 {
        let _ = write!(out, "{days} days ");
    }
    let sod = secs % 86_400;
    let _ = write!(
        out,
        "{:02}:{:02}:{:02}",
        sod / 3600,
        (sod % 3600) / 60,
        sod % 60
    );
    if frac != 0 {
        let digits = match unit {
            TimeUnit::Second => 0,
            TimeUnit::Millisecond => 3,
            TimeUnit::Microsecond => 6,
            TimeUnit::Nanosecond => 9,
        };
        let mut f = format!("{frac:0digits$}");
        while f.ends_with('0') {
            f.pop();
        }
        out.push('.');
        out.push_str(&f);
    }
    out
}

/// Short type name for a type the exporter does not render.
#[must_use]
pub fn type_label(data_type: &DataType) -> String {
    let full = data_type.to_string();
    match full.find('(') {
        Some(i) => full[..i].to_owned(),
        None => full,
    }
}

/// The value at `row` of `array`. A row out of range reads as NULL.
#[must_use]
#[allow(clippy::too_many_lines)] // One arm per Arrow type.
pub fn cell(array: &ArrayRef, row: usize) -> CellValue<'_> {
    if row >= array.len() || array.is_null(row) {
        return CellValue::Null;
    }
    let any = array.as_any();
    macro_rules! get {
        ($ty:ty) => {
            any.downcast_ref::<$ty>().map(|a| a.value(row))
        };
    }
    let value =
        match array.data_type() {
            DataType::Null => Some(CellValue::Null),
            DataType::Boolean => get!(BooleanArray).map(CellValue::Bool),
            DataType::Int8 => get!(Int8Array).map(|v| CellValue::Int(i64::from(v))),
            DataType::Int16 => get!(Int16Array).map(|v| CellValue::Int(i64::from(v))),
            DataType::Int32 => get!(Int32Array).map(|v| CellValue::Int(i64::from(v))),
            DataType::Int64 => get!(Int64Array).map(CellValue::Int),
            DataType::UInt8 => get!(UInt8Array).map(|v| CellValue::UInt(u64::from(v))),
            DataType::UInt16 => get!(UInt16Array).map(|v| CellValue::UInt(u64::from(v))),
            DataType::UInt32 => get!(UInt32Array).map(|v| CellValue::UInt(u64::from(v))),
            DataType::UInt64 => get!(UInt64Array).map(CellValue::UInt),
            DataType::Float16 => get!(Float16Array).map(|v| CellValue::Float(v.to_f64())),
            DataType::Float32 => get!(Float32Array).map(|v| CellValue::Float(f64::from(v))),
            DataType::Float64 => get!(Float64Array).map(CellValue::Float),
            DataType::Decimal128(_, scale) => get!(Decimal128Array)
                .map(|v| CellValue::Decimal(decimal_text(&v.to_string(), *scale))),
            DataType::Decimal256(_, scale) => get!(Decimal256Array)
                .map(|v| CellValue::Decimal(decimal_text(&v.to_string(), *scale))),
            DataType::Utf8 => get!(StringArray).map(CellValue::Text),
            DataType::LargeUtf8 => get!(LargeStringArray).map(CellValue::Text),
            DataType::Utf8View => get!(StringViewArray).map(CellValue::Text),
            DataType::Binary => get!(BinaryArray).map(CellValue::Bytes),
            DataType::LargeBinary => get!(LargeBinaryArray).map(CellValue::Bytes),
            DataType::BinaryView => get!(BinaryViewArray).map(CellValue::Bytes),
            DataType::FixedSizeBinary(_) => get!(FixedSizeBinaryArray).map(CellValue::Bytes),
            DataType::Date32 => get!(Date32Array).map(|v| CellValue::Date(i64::from(v))),
            DataType::Date64 => get!(Date64Array).map(|v| {
                let (secs, nanos) = split_units(v, TimeUnit::Millisecond);
                CellValue::Timestamp {
                    secs,
                    nanos,
                    utc: false,
                }
            }),
            DataType::Time32(TimeUnit::Second) => {
                get!(Time32SecondArray).map(|v| time(i64::from(v), TimeUnit::Second))
            }
            DataType::Time32(TimeUnit::Millisecond) => {
                get!(Time32MillisecondArray).map(|v| time(i64::from(v), TimeUnit::Millisecond))
            }
            DataType::Time64(TimeUnit::Microsecond) => {
                get!(Time64MicrosecondArray).map(|v| time(v, TimeUnit::Microsecond))
            }
            DataType::Time64(TimeUnit::Nanosecond) => {
                get!(Time64NanosecondArray).map(|v| time(v, TimeUnit::Nanosecond))
            }
            DataType::Timestamp(unit, tz) => {
                let v = match unit {
                    TimeUnit::Second => get!(TimestampSecondArray),
                    TimeUnit::Millisecond => get!(TimestampMillisecondArray),
                    TimeUnit::Microsecond => get!(TimestampMicrosecondArray),
                    TimeUnit::Nanosecond => get!(TimestampNanosecondArray),
                };
                v.map(|v| {
                    let (secs, nanos) = split_units(v, *unit);
                    CellValue::Timestamp {
                        secs,
                        nanos,
                        utc: tz.is_some(),
                    }
                })
            }
            DataType::Duration(unit) => {
                let v = match unit {
                    TimeUnit::Second => get!(DurationSecondArray),
                    TimeUnit::Millisecond => get!(DurationMillisecondArray),
                    TimeUnit::Microsecond => get!(DurationMicrosecondArray),
                    TimeUnit::Nanosecond => get!(DurationNanosecondArray),
                };
                v.map(|v| CellValue::Duration(duration_text(v, *unit)))
            }
            _ => None,
        };
    value.unwrap_or_else(|| CellValue::Unsupported(type_label(array.data_type())))
}

fn time(value: i64, unit: TimeUnit) -> CellValue<'static> {
    let (secs, nanos) = split_units(value, unit);
    CellValue::Time {
        secs: secs.rem_euclid(86_400),
        nanos,
    }
}

/// Proleptic Gregorian date from days since 1970-01-01 (H. Hinnant's `civil_from_days`).
#[must_use]
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (
        year,
        u32::try_from(month).unwrap_or(1),
        u32::try_from(day).unwrap_or(1),
    )
}

/// Days since 1970-01-01 of a proleptic Gregorian date (H. Hinnant's `days_from_civil`).
#[must_use]
pub fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = i64::from(month);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// One piece of a [`TimePattern`].
#[derive(Clone, Debug, PartialEq, Eq)]
enum PatternItem {
    Literal(String),
    Year,
    Month,
    Day,
    Hour,
    Minute,
    Second,
    /// Fraction of a second with this many digits (1 to 9).
    Fraction(usize),
    /// Fraction with trailing zeros removed (and the dot before it when it is all zeros).
    FractionTrimmed,
}

/// A date / time pattern: `%Y` year, `%m` month, `%d` day, `%H` hour, `%M` minute, `%S`
/// second, `%3f` / `%6f` / `%9f` a fraction with that many digits, `%f` the fraction with
/// trailing zeros removed (the preceding `.` too when it is zero), `%%` a percent sign; any
/// other character is copied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimePattern {
    items: Vec<PatternItem>,
}

/// Longest accepted pattern, in bytes.
const MAX_PATTERN_LEN: usize = 128;

impl TimePattern {
    /// Parses a pattern.
    ///
    /// # Errors
    /// [`TransferError::InvalidOption`] for an unknown `%` directive or a pattern over 128
    /// bytes.
    pub fn parse(pattern: &str) -> Result<Self, TransferError> {
        if pattern.len() > MAX_PATTERN_LEN {
            return Err(TransferError::InvalidOption(
                "date pattern longer than 128 bytes".into(),
            ));
        }
        let mut items = Vec::new();
        let mut lit = String::new();
        let mut chars = pattern.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '%' {
                lit.push(c);
                continue;
            }
            let item = match chars.next() {
                Some('%') => {
                    lit.push('%');
                    continue;
                }
                Some('Y') => PatternItem::Year,
                Some('m') => PatternItem::Month,
                Some('d') => PatternItem::Day,
                Some('H') => PatternItem::Hour,
                Some('M') => PatternItem::Minute,
                Some('S') => PatternItem::Second,
                Some('f') => PatternItem::FractionTrimmed,
                Some(d @ '1'..='9') if chars.peek() == Some(&'f') => {
                    chars.next();
                    PatternItem::Fraction(usize::from(d as u8 - b'0'))
                }
                _ => {
                    return Err(TransferError::InvalidOption(
                        "unknown directive in date pattern (use %Y %m %d %H %M %S %f %3f %6f %9f %%)"
                            .into(),
                    ));
                }
            };
            if !lit.is_empty() {
                items.push(PatternItem::Literal(std::mem::take(&mut lit)));
            }
            items.push(item);
        }
        if !lit.is_empty() {
            items.push(PatternItem::Literal(lit));
        }
        Ok(Self { items })
    }

    fn write(&self, out: &mut String, date: Option<(i64, u32, u32)>, clock: Option<(i64, u32)>) {
        let (y, mo, d) = date.unwrap_or((1970, 1, 1));
        let (sod, nanos) = clock.unwrap_or((0, 0));
        let mut i = 0;
        while i < self.items.len() {
            match &self.items[i] {
                PatternItem::Literal(s) => {
                    // A `.` right before a trimmed, zero fraction is dropped with it.
                    let drop_dot = nanos == 0
                        && s.ends_with('.')
                        && self.items.get(i + 1) == Some(&PatternItem::FractionTrimmed);
                    if drop_dot {
                        out.push_str(&s[..s.len() - 1]);
                    } else {
                        out.push_str(s);
                    }
                }
                PatternItem::Year => {
                    if (0..=9999).contains(&y) {
                        let _ = write!(out, "{y:04}");
                    } else {
                        let _ = write!(out, "{y:+05}");
                    }
                }
                PatternItem::Month => {
                    let _ = write!(out, "{mo:02}");
                }
                PatternItem::Day => {
                    let _ = write!(out, "{d:02}");
                }
                PatternItem::Hour => {
                    let _ = write!(out, "{:02}", sod / 3600);
                }
                PatternItem::Minute => {
                    let _ = write!(out, "{:02}", (sod % 3600) / 60);
                }
                PatternItem::Second => {
                    let _ = write!(out, "{:02}", sod % 60);
                }
                PatternItem::Fraction(digits) => {
                    let s = format!("{nanos:09}");
                    out.push_str(&s[..*digits]);
                }
                PatternItem::FractionTrimmed => {
                    if nanos != 0 {
                        let mut s = format!("{nanos:09}");
                        while s.ends_with('0') {
                            s.pop();
                        }
                        out.push_str(&s);
                    }
                }
            }
            i += 1;
        }
    }
}

/// How binary values are written as text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum BinaryText {
    /// `0x` followed by lower-case hex digits.
    #[default]
    Hex,
    /// Standard Base64 with padding.
    Base64,
}

/// How values become text in the text formats.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct TextOptions {
    /// Text written for NULL.
    pub null_text: String,
    /// Pattern of dates (default `%Y-%m-%d`).
    pub date: TimePattern,
    /// Pattern of times of day (default `%H:%M:%S.%f`).
    pub time: TimePattern,
    /// Pattern of timestamps (default `%Y-%m-%d %H:%M:%S.%f`). A zoned timestamp (an instant)
    /// is written in UTC; [`Self::utc_suffix`] is appended to it.
    pub timestamp: TimePattern,
    /// Appended to a zoned timestamp (default `Z`).
    pub utc_suffix: String,
    /// Decimal separator of floats and decimals (default `.`).
    pub decimal_separator: char,
    /// Binary values.
    pub binary: BinaryText,
}

impl Default for TextOptions {
    fn default() -> Self {
        Self {
            null_text: String::new(),
            date: TimePattern::parse("%Y-%m-%d").unwrap_or(TimePattern { items: Vec::new() }),
            time: TimePattern::parse("%H:%M:%S.%f").unwrap_or(TimePattern { items: Vec::new() }),
            timestamp: TimePattern::parse("%Y-%m-%d %H:%M:%S.%f")
                .unwrap_or(TimePattern { items: Vec::new() }),
            utc_suffix: "Z".to_owned(),
            decimal_separator: '.',
            binary: BinaryText::Hex,
        }
    }
}

const HEX: &[u8; 16] = b"0123456789abcdef";
const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Appends `bytes` as `0x…` lower-case hex.
pub fn write_hex(out: &mut String, bytes: &[u8]) {
    out.reserve(2 + bytes.len() * 2);
    out.push_str("0x");
    for b in bytes {
        out.push(char::from(HEX[usize::from(b >> 4)]));
        out.push(char::from(HEX[usize::from(b & 0x0f)]));
    }
}

/// Appends `bytes` as standard Base64 with padding.
pub fn write_base64(out: &mut String, bytes: &[u8]) {
    out.reserve(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(char::from(B64[(n >> 18) as usize & 63]));
        out.push(char::from(B64[(n >> 12) as usize & 63]));
        out.push(if chunk.len() > 1 {
            char::from(B64[(n >> 6) as usize & 63])
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            char::from(B64[n as usize & 63])
        } else {
            '='
        });
    }
}

/// Text of a finite or non-finite float: shortest round-trip form, `NaN`, `Infinity`,
/// `-Infinity`.
pub(crate) fn float_text(out: &mut String, f: f64) {
    if f.is_nan() {
        out.push_str("NaN");
    } else if f.is_infinite() {
        out.push_str(if f > 0.0 { "Infinity" } else { "-Infinity" });
    } else {
        let _ = write!(out, "{f}");
    }
}

impl TextOptions {
    /// Appends the text form of `value` (NULL appends [`Self::null_text`]).
    pub fn write(&self, out: &mut String, value: &CellValue<'_>) {
        match value {
            CellValue::Null => out.push_str(&self.null_text),
            CellValue::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            CellValue::Int(i) => {
                let _ = write!(out, "{i}");
            }
            CellValue::UInt(u) => {
                let _ = write!(out, "{u}");
            }
            CellValue::Float(f) => {
                let start = out.len();
                float_text(out, *f);
                self.swap_separator(out, start);
            }
            CellValue::Decimal(d) => {
                let start = out.len();
                out.push_str(d);
                self.swap_separator(out, start);
            }
            CellValue::Text(s) => out.push_str(s),
            CellValue::Bytes(b) => match self.binary {
                BinaryText::Hex => write_hex(out, b),
                BinaryText::Base64 => write_base64(out, b),
            },
            CellValue::Date(days) => {
                self.date.write(out, Some(civil_from_days(*days)), None);
            }
            CellValue::Time { secs, nanos } => self.time.write(out, None, Some((*secs, *nanos))),
            CellValue::Timestamp { secs, nanos, utc } => {
                let days = secs.div_euclid(86_400);
                let sod = secs.rem_euclid(86_400);
                self.timestamp
                    .write(out, Some(civil_from_days(days)), Some((sod, *nanos)));
                if *utc {
                    out.push_str(&self.utc_suffix);
                }
            }
            CellValue::Duration(s) => out.push_str(s),
            CellValue::Unsupported(label) => {
                out.push('<');
                out.push_str(label);
                out.push('>');
            }
        }
    }

    fn swap_separator(&self, out: &mut String, start: usize) {
        if self.decimal_separator == '.' {
            return;
        }
        if let Some(pos) = out[start..].find('.') {
            let at = start + pos;
            out.replace_range(at..=at, self.decimal_separator.encode_utf8(&mut [0; 4]));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_round_trip() {
        for days in [-1_000_000, -1, 0, 1, 10_957, 19_000, 2_932_896] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
    }

    #[test]
    fn patterns() {
        let p = TimePattern::parse("%d/%m/%Y %H:%M:%S.%f").unwrap();
        let mut out = String::new();
        p.write(&mut out, Some((2024, 2, 29)), Some((3661, 0)));
        assert_eq!(out, "29/02/2024 01:01:01");
        out.clear();
        p.write(&mut out, Some((2024, 2, 29)), Some((3661, 500_000_000)));
        assert_eq!(out, "29/02/2024 01:01:01.5");
        out.clear();
        TimePattern::parse("%3f%%")
            .unwrap()
            .write(&mut out, None, Some((0, 123_456_789)));
        assert_eq!(out, "123%");
        assert!(TimePattern::parse("%Q").is_err());
        assert!(TimePattern::parse("%").is_err());
    }

    #[test]
    fn base64() {
        let mut s = String::new();
        write_base64(&mut s, b"any carnal pleas");
        assert_eq!(s, "YW55IGNhcm5hbCBwbGVhcw==");
        s.clear();
        write_base64(&mut s, b"ab");
        assert_eq!(s, "YWI=");
    }

    #[test]
    fn decimal_separator() {
        let opts = TextOptions {
            decimal_separator: ',',
            ..TextOptions::default()
        };
        let mut s = String::new();
        opts.write(&mut s, &CellValue::Float(1.5));
        opts.write(&mut s, &CellValue::Text(";"));
        opts.write(&mut s, &CellValue::Decimal("-0.25".into()));
        assert_eq!(s, "1,5;-0,25");
    }
}
