//! Type-aware rendering of Arrow values (F-DATA-01): NULL, numbers right-aligned, dates and
//! times, binary as a hex preview, JSON, booleans, long text cut with an ellipsis.
//!
//! Two forms exist for every value:
//! - the **display** form ([`format_display`]): one line, at most
//!   [`DisplayOptions::max_chars`] characters, control and bidirectional characters made
//!   visible, so a cell can never hide or reorder text (Trojan Source);
//! - the **copy** form ([`write_copy`]): the value as delivered by the driver (dates and binary in
//!   their text form), used by copy. It is never cut by the grid; a value the driver cut is copied
//!   as its kept prefix and reported (see [`crate::copy`]).
//!
//! Formatting never logs a value (AGENTS.md).

use std::fmt::Write as _;

use ninesql_core::driver_api::ENGINE_TYPE_METADATA_KEY;
use ninesql_core::driver_api::arrow_array::{
    Array, ArrayRef, BinaryArray, BinaryViewArray, BooleanArray, Date32Array, Date64Array,
    Decimal128Array, Decimal256Array, DurationMicrosecondArray, DurationMillisecondArray,
    DurationNanosecondArray, DurationSecondArray, FixedSizeBinaryArray, Float16Array, Float32Array,
    Float64Array, Int8Array, Int16Array, Int32Array, Int64Array, LargeBinaryArray,
    LargeStringArray, StringArray, StringViewArray, Time32MillisecondArray, Time32SecondArray,
    Time64MicrosecondArray, Time64NanosecondArray, TimestampMicrosecondArray,
    TimestampMillisecondArray, TimestampNanosecondArray, TimestampSecondArray, UInt8Array,
    UInt16Array, UInt32Array, UInt64Array,
};
use ninesql_core::driver_api::arrow_schema::{DataType, Field, TimeUnit};

/// Horizontal alignment of a cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    /// Text, dates, binary, JSON.
    Left,
    /// Booleans.
    Center,
    /// Numbers.
    Right,
}

/// What a column holds, derived once per result from its Arrow type and engine type name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueKind {
    /// Integers, floats, decimals (also decimals the driver sends as text, `ninesql.type`
    /// `numeric` / `decimal`).
    Number,
    /// Booleans.
    Boolean,
    /// Dates, times, timestamps, durations.
    Temporal,
    /// Binary values, shown as a hex preview.
    Binary,
    /// JSON text (`json`, `jsonb`), shown on one line.
    Json,
    /// Any other text.
    Text,
    /// An Arrow type the grid does not render yet (lists, structs, maps…): shown as a type
    /// label, never as a fake value (I8).
    Unsupported,
}

impl ValueKind {
    /// Classifies a field from its Arrow type and its `ninesql.type` metadata.
    #[must_use]
    pub fn of_field(field: &Field) -> Self {
        let engine_type = field
            .metadata()
            .get(ENGINE_TYPE_METADATA_KEY)
            .map(|t| t.to_ascii_lowercase());
        Self::of(field.data_type(), engine_type.as_deref())
    }

    /// Classifies an Arrow type with an optional (lower-case) engine type name.
    #[must_use]
    pub fn of(data_type: &DataType, engine_type: Option<&str>) -> Self {
        match data_type {
            DataType::Int8
            | DataType::Int16
            | DataType::Int32
            | DataType::Int64
            | DataType::UInt8
            | DataType::UInt16
            | DataType::UInt32
            | DataType::UInt64
            | DataType::Float16
            | DataType::Float32
            | DataType::Float64
            | DataType::Decimal128(..)
            | DataType::Decimal256(..) => Self::Number,
            DataType::Boolean => Self::Boolean,
            DataType::Date32
            | DataType::Date64
            | DataType::Time32(_)
            | DataType::Time64(_)
            | DataType::Timestamp(..)
            | DataType::Duration(_) => Self::Temporal,
            DataType::Binary
            | DataType::LargeBinary
            | DataType::BinaryView
            | DataType::FixedSizeBinary(_) => Self::Binary,
            DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View => match engine_type {
                Some(t) if t == "json" || t == "jsonb" => Self::Json,
                Some(t)
                    if t.starts_with("numeric")
                        || t.starts_with("decimal")
                        || t.starts_with("number") =>
                {
                    Self::Number
                }
                _ => Self::Text,
            },
            DataType::Null => Self::Text,
            _ => Self::Unsupported,
        }
    }

    /// Alignment of the kind.
    #[must_use]
    pub fn align(self) -> Align {
        match self {
            Self::Number => Align::Right,
            Self::Boolean => Align::Center,
            _ => Align::Left,
        }
    }
}

/// Display settings of the grid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayOptions {
    /// Longest display text, in characters; longer values end with `…`.
    pub max_chars: usize,
    /// Bytes shown in a binary preview.
    pub binary_preview_bytes: usize,
    /// Text of a NULL cell.
    pub null_text: &'static str,
}

impl Default for DisplayOptions {
    fn default() -> Self {
        Self {
            max_chars: 200,
            binary_preview_bytes: 16,
            null_text: "NULL",
        }
    }
}

/// One rendered cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellText {
    /// One-line display text.
    pub text: String,
    /// `true` for NULL (rendered muted).
    pub is_null: bool,
    /// `true` when the display text was cut by the grid (ellipsis added). This is display
    /// cutting only; values the driver cut are flagged per column (`ninesql.truncated`).
    pub elided: bool,
    /// Alignment.
    pub align: Align,
}

/// The ellipsis appended to cut display text.
pub const ELLIPSIS: char = '…';

/// Display form of the value at `row` of `array`.
#[must_use]
pub fn format_display(
    array: &ArrayRef,
    row: usize,
    kind: ValueKind,
    options: &DisplayOptions,
) -> CellText {
    let align = kind.align();
    if row >= array.len() || array.is_null(row) {
        return CellText {
            text: options.null_text.to_owned(),
            is_null: true,
            elided: false,
            align,
        };
    }
    let mut raw = String::new();
    let binary_limit = Some(options.binary_preview_bytes);
    write_value(&mut raw, array, row, binary_limit);
    if kind == ValueKind::Json {
        raw = collapse_whitespace(&raw);
    }
    let (text, elided) = display_line(&raw, options.max_chars);
    CellText {
        text,
        is_null: false,
        elided,
        align,
    }
}

/// Appends the copy form of the value at `row` to `out`. NULL appends `null_text`.
pub fn write_copy(out: &mut String, array: &ArrayRef, row: usize, null_text: &str) {
    if row >= array.len() || array.is_null(row) {
        out.push_str(null_text);
        return;
    }
    write_value(out, array, row, None);
}

/// Writes a non-null value. `binary_limit` cuts binary previews (display only).
fn write_value(out: &mut String, array: &ArrayRef, row: usize, binary_limit: Option<usize>) {
    macro_rules! prim {
        ($ty:ty) => {
            if let Some(a) = array.as_any().downcast_ref::<$ty>() {
                let _ = write!(out, "{}", a.value(row));
                return;
            }
        };
    }
    match array.data_type() {
        DataType::Boolean => {
            if let Some(a) = array.as_any().downcast_ref::<BooleanArray>() {
                out.push_str(if a.value(row) { "true" } else { "false" });
                return;
            }
        }
        DataType::Int8 => prim!(Int8Array),
        DataType::Int16 => prim!(Int16Array),
        DataType::Int32 => prim!(Int32Array),
        DataType::Int64 => prim!(Int64Array),
        DataType::UInt8 => prim!(UInt8Array),
        DataType::UInt16 => prim!(UInt16Array),
        DataType::UInt32 => prim!(UInt32Array),
        DataType::UInt64 => prim!(UInt64Array),
        DataType::Float16 => prim!(Float16Array),
        DataType::Float32 => prim!(Float32Array),
        DataType::Float64 => prim!(Float64Array),
        DataType::Decimal128(_, scale) => {
            if let Some(a) = array.as_any().downcast_ref::<Decimal128Array>() {
                write_decimal(out, &a.value(row).to_string(), *scale);
                return;
            }
        }
        DataType::Decimal256(_, scale) => {
            if let Some(a) = array.as_any().downcast_ref::<Decimal256Array>() {
                write_decimal(out, &a.value(row).to_string(), *scale);
                return;
            }
        }
        DataType::Utf8 => {
            if let Some(a) = array.as_any().downcast_ref::<StringArray>() {
                out.push_str(a.value(row));
                return;
            }
        }
        DataType::LargeUtf8 => {
            if let Some(a) = array.as_any().downcast_ref::<LargeStringArray>() {
                out.push_str(a.value(row));
                return;
            }
        }
        DataType::Utf8View => {
            if let Some(a) = array.as_any().downcast_ref::<StringViewArray>() {
                out.push_str(a.value(row));
                return;
            }
        }
        DataType::Binary => {
            if let Some(a) = array.as_any().downcast_ref::<BinaryArray>() {
                write_hex(out, a.value(row), binary_limit);
                return;
            }
        }
        DataType::LargeBinary => {
            if let Some(a) = array.as_any().downcast_ref::<LargeBinaryArray>() {
                write_hex(out, a.value(row), binary_limit);
                return;
            }
        }
        DataType::BinaryView => {
            if let Some(a) = array.as_any().downcast_ref::<BinaryViewArray>() {
                write_hex(out, a.value(row), binary_limit);
                return;
            }
        }
        DataType::FixedSizeBinary(_) => {
            if let Some(a) = array.as_any().downcast_ref::<FixedSizeBinaryArray>() {
                write_hex(out, a.value(row), binary_limit);
                return;
            }
        }
        DataType::Date32 => {
            if let Some(a) = array.as_any().downcast_ref::<Date32Array>() {
                write_date(out, i64::from(a.value(row)));
                return;
            }
        }
        DataType::Date64 => {
            if let Some(a) = array.as_any().downcast_ref::<Date64Array>() {
                write_timestamp(out, i128::from(a.value(row)), TimeUnit::Millisecond, None);
                return;
            }
        }
        DataType::Time32(TimeUnit::Second) => {
            if let Some(a) = array.as_any().downcast_ref::<Time32SecondArray>() {
                write_time_of_day(out, i128::from(a.value(row)), TimeUnit::Second);
                return;
            }
        }
        DataType::Time32(TimeUnit::Millisecond) => {
            if let Some(a) = array.as_any().downcast_ref::<Time32MillisecondArray>() {
                write_time_of_day(out, i128::from(a.value(row)), TimeUnit::Millisecond);
                return;
            }
        }
        DataType::Time64(TimeUnit::Microsecond) => {
            if let Some(a) = array.as_any().downcast_ref::<Time64MicrosecondArray>() {
                write_time_of_day(out, i128::from(a.value(row)), TimeUnit::Microsecond);
                return;
            }
        }
        DataType::Time64(TimeUnit::Nanosecond) => {
            if let Some(a) = array.as_any().downcast_ref::<Time64NanosecondArray>() {
                write_time_of_day(out, i128::from(a.value(row)), TimeUnit::Nanosecond);
                return;
            }
        }
        DataType::Timestamp(unit, tz) => {
            let tz = tz.as_deref();
            let v = match unit {
                TimeUnit::Second => array
                    .as_any()
                    .downcast_ref::<TimestampSecondArray>()
                    .map(|a| a.value(row)),
                TimeUnit::Millisecond => array
                    .as_any()
                    .downcast_ref::<TimestampMillisecondArray>()
                    .map(|a| a.value(row)),
                TimeUnit::Microsecond => array
                    .as_any()
                    .downcast_ref::<TimestampMicrosecondArray>()
                    .map(|a| a.value(row)),
                TimeUnit::Nanosecond => array
                    .as_any()
                    .downcast_ref::<TimestampNanosecondArray>()
                    .map(|a| a.value(row)),
            };
            if let Some(v) = v {
                write_timestamp(out, i128::from(v), *unit, tz);
                return;
            }
        }
        DataType::Duration(unit) => {
            let v = match unit {
                TimeUnit::Second => array
                    .as_any()
                    .downcast_ref::<DurationSecondArray>()
                    .map(|a| a.value(row)),
                TimeUnit::Millisecond => array
                    .as_any()
                    .downcast_ref::<DurationMillisecondArray>()
                    .map(|a| a.value(row)),
                TimeUnit::Microsecond => array
                    .as_any()
                    .downcast_ref::<DurationMicrosecondArray>()
                    .map(|a| a.value(row)),
                TimeUnit::Nanosecond => array
                    .as_any()
                    .downcast_ref::<DurationNanosecondArray>()
                    .map(|a| a.value(row)),
            };
            if let Some(v) = v {
                write_duration(out, i128::from(v), *unit);
                return;
            }
        }
        _ => {}
    }
    // Honest label for a type the grid does not render (I8).
    let _ = write!(out, "<{}>", type_label(array.data_type()));
}

/// Short type name for unsupported values and headers.
#[must_use]
pub fn type_label(data_type: &DataType) -> String {
    let full = data_type.to_string();
    // "List(Field { … })" → "List"; keep simple names whole.
    match full.find('(') {
        Some(i) => full[..i].to_owned(),
        None => full,
    }
}

fn write_decimal(out: &mut String, digits: &str, scale: i8) {
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
        return;
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
}

const HEX: &[u8; 16] = b"0123456789abcdef";

fn write_hex(out: &mut String, bytes: &[u8], limit: Option<usize>) {
    out.push_str("0x");
    let shown = limit.map_or(bytes.len(), |l| l.min(bytes.len()));
    out.reserve(shown * 2);
    for b in &bytes[..shown] {
        out.push(char::from(HEX[usize::from(b >> 4)]));
        out.push(char::from(HEX[usize::from(b & 0x0f)]));
    }
    if shown < bytes.len() {
        let _ = write!(out, "{ELLIPSIS} ({} bytes)", bytes.len());
    }
}

/// Proleptic Gregorian date from days since 1970-01-01 (H. Hinnant's `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    // `day` is 1..=31 and `month` 1..=12 by construction.
    (
        year,
        u32::try_from(month).unwrap_or(1),
        u32::try_from(day).unwrap_or(1),
    )
}

fn write_date(out: &mut String, days: i64) {
    let (y, m, d) = civil_from_days(days);
    if (0..=9999).contains(&y) {
        let _ = write!(out, "{y:04}-{m:02}-{d:02}");
    } else {
        let _ = write!(out, "{y:+05}-{m:02}-{d:02}");
    }
}

fn unit_per_second(unit: TimeUnit) -> i128 {
    match unit {
        TimeUnit::Second => 1,
        TimeUnit::Millisecond => 1_000,
        TimeUnit::Microsecond => 1_000_000,
        TimeUnit::Nanosecond => 1_000_000_000,
    }
}

fn frac_digits(unit: TimeUnit) -> usize {
    match unit {
        TimeUnit::Second => 0,
        TimeUnit::Millisecond => 3,
        TimeUnit::Microsecond => 6,
        TimeUnit::Nanosecond => 9,
    }
}

/// `HH:MM:SS[.fff…]` with trailing zero fractions dropped.
fn write_clock(out: &mut String, secs_of_day: i128, frac: i128, unit: TimeUnit) {
    let h = secs_of_day / 3600;
    let m = (secs_of_day % 3600) / 60;
    let s = secs_of_day % 60;
    let _ = write!(out, "{h:02}:{m:02}:{s:02}");
    if frac != 0 {
        let digits = frac_digits(unit);
        let mut f = format!("{frac:0digits$}");
        while f.ends_with('0') {
            f.pop();
        }
        out.push('.');
        out.push_str(&f);
    }
}

fn write_time_of_day(out: &mut String, value: i128, unit: TimeUnit) {
    let per = unit_per_second(unit);
    let secs = value.div_euclid(per);
    let frac = value.rem_euclid(per);
    write_clock(out, secs, frac, unit);
}

fn write_timestamp(out: &mut String, value: i128, unit: TimeUnit, tz: Option<&str>) {
    let per = unit_per_second(unit);
    let secs = value.div_euclid(per);
    let frac = value.rem_euclid(per);
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400);
    write_date(out, i64::try_from(days).unwrap_or(i64::MAX));
    out.push(' ');
    write_clock(out, secs_of_day, frac, unit);
    // Arrow stores a zoned timestamp as a UTC instant; it is shown in UTC, labelled.
    if tz.is_some() {
        out.push_str(" UTC");
    }
}

fn write_duration(out: &mut String, value: i128, unit: TimeUnit) {
    let per = unit_per_second(unit);
    if value < 0 {
        out.push('-');
    }
    let v = value.abs();
    let secs = v / per;
    let frac = v % per;
    let days = secs / 86_400;
    if days > 0 {
        let _ = write!(out, "{days} days ");
    }
    write_clock(out, secs % 86_400, frac, unit);
}

/// One display line of at most `max_chars` characters: control characters, line breaks and
/// bidirectional formatting characters are replaced with visible symbols, and text longer than
/// `max_chars` ends with [`ELLIPSIS`]. Returns the line and whether it was cut.
#[must_use]
pub fn display_line(raw: &str, max_chars: usize) -> (String, bool) {
    let mut out = String::with_capacity(raw.len().min(max_chars.saturating_mul(4)));
    for (count, ch) in raw.chars().enumerate() {
        if count >= max_chars {
            out.push(ELLIPSIS);
            return (out, true);
        }
        match ch {
            '\n' => out.push('↵'),
            '\r' => out.push('␍'),
            '\t' => out.push('→'),
            c if is_bidi_control(c) => {
                let _ = write!(out, "<U+{:04X}>", u32::from(c));
            }
            c if c.is_control() => out.push('\u{FFFD}'),
            c => out.push(c),
        }
    }
    (out, false)
}

/// Bidirectional formatting characters that can reorder what a cell shows (Trojan Source).
#[must_use]
pub fn is_bidi_control(c: char) -> bool {
    matches!(
        c,
        '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
    )
}

fn collapse_whitespace(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut in_ws = false;
    for ch in raw.chars() {
        if ch.is_whitespace() {
            if !in_ws {
                out.push(' ');
            }
            in_ws = true;
        } else {
            out.push(ch);
            in_ws = false;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ninesql_core::driver_api::arrow_array::types::Decimal128Type;
    use ninesql_core::driver_api::arrow_array::{ListArray, PrimitiveArray};

    use super::*;

    fn show(array: ArrayRef, kind: ValueKind) -> String {
        format_display(&array, 0, kind, &DisplayOptions::default()).text
    }

    #[test]
    fn kinds_and_alignment() {
        assert_eq!(ValueKind::of(&DataType::Int32, None).align(), Align::Right);
        assert_eq!(
            ValueKind::of(&DataType::Utf8, Some("numeric")),
            ValueKind::Number
        );
        assert_eq!(
            ValueKind::of(&DataType::Utf8, Some("jsonb")),
            ValueKind::Json
        );
        assert_eq!(
            ValueKind::of(&DataType::Utf8, Some("uuid")),
            ValueKind::Text
        );
        assert_eq!(
            ValueKind::of(&DataType::Boolean, None).align(),
            Align::Center
        );
        assert_eq!(
            ValueKind::of(
                &DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())),
                None
            ),
            ValueKind::Temporal
        );
        let field = Field::new("x", DataType::Utf8, true)
            .with_metadata([(ENGINE_TYPE_METADATA_KEY.to_owned(), "JSON".to_owned())]);
        assert_eq!(ValueKind::of_field(&field), ValueKind::Json);
    }

    #[test]
    fn null_and_numbers() {
        let a: ArrayRef = Arc::new(Int64Array::from(vec![None, Some(-42)]));
        let c = format_display(&a, 0, ValueKind::Number, &DisplayOptions::default());
        assert!(c.is_null);
        assert_eq!(c.text, "NULL");
        let c = format_display(&a, 1, ValueKind::Number, &DisplayOptions::default());
        assert_eq!((c.text.as_str(), c.align), ("-42", Align::Right));
        let f: ArrayRef = Arc::new(Float64Array::from(vec![0.1]));
        assert_eq!(show(f, ValueKind::Number), "0.1");
    }

    #[test]
    fn decimals() {
        let mk = |v: i128, p: u8, s: i8| -> ArrayRef {
            Arc::new(
                PrimitiveArray::<Decimal128Type>::from(vec![v])
                    .with_precision_and_scale(p, s)
                    .unwrap(),
            )
        };
        assert_eq!(show(mk(12345, 10, 2), ValueKind::Number), "123.45");
        assert_eq!(show(mk(-5, 10, 3), ValueKind::Number), "-0.005");
        assert_eq!(show(mk(7, 10, -2), ValueKind::Number), "700");
        assert_eq!(show(mk(0, 10, 0), ValueKind::Number), "0");
    }

    #[test]
    fn dates_and_times() {
        let d: ArrayRef = Arc::new(Date32Array::from(vec![19_723, -1, 0]));
        assert_eq!(show(d.clone(), ValueKind::Temporal), "2024-01-01");
        let opts = DisplayOptions::default();
        assert_eq!(
            format_display(&d, 1, ValueKind::Temporal, &opts).text,
            "1969-12-31"
        );
        let t: ArrayRef = Arc::new(Time64MicrosecondArray::from(vec![
            (13 * 3600 + 5 * 60 + 9) * 1_000_000 + 120_000,
        ]));
        assert_eq!(show(t, ValueKind::Temporal), "13:05:09.12");
        let ts: ArrayRef = Arc::new(
            TimestampMicrosecondArray::from(vec![1_704_067_200_000_000 + 1]).with_timezone("UTC"),
        );
        assert_eq!(
            show(ts, ValueKind::Temporal),
            "2024-01-01 00:00:00.000001 UTC"
        );
        let ts: ArrayRef = Arc::new(TimestampSecondArray::from(vec![-1]));
        assert_eq!(show(ts, ValueKind::Temporal), "1969-12-31 23:59:59");
        let du: ArrayRef = Arc::new(DurationSecondArray::from(vec![-(86_400 + 61)]));
        assert_eq!(show(du, ValueKind::Temporal), "-1 days 00:01:01");
    }

    #[test]
    fn civil_dates_roundtrip_known_points() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
        assert_eq!(civil_from_days(-719_468), (0, 3, 1));
        let mut out = String::new();
        write_date(&mut out, -719_469 - 366);
        assert!(out.starts_with('-'), "{out}");
    }

    #[test]
    fn binary_preview_and_copy() {
        let bytes: Vec<u8> = (0u8..40).collect();
        let a: ArrayRef = Arc::new(BinaryArray::from(vec![bytes.as_slice()]));
        let c = show(a.clone(), ValueKind::Binary);
        assert_eq!(c, "0x000102030405060708090a0b0c0d0e0f… (40 bytes)");
        let mut copy = String::new();
        write_copy(&mut copy, &a, 0, "");
        assert_eq!(copy.len(), 2 + 80);
    }

    #[test]
    fn long_text_is_cut_and_controls_are_visible() {
        let long = "é".repeat(500);
        let a: ArrayRef = Arc::new(StringArray::from(vec![long.as_str()]));
        let c = format_display(&a, 0, ValueKind::Text, &DisplayOptions::default());
        assert!(c.elided);
        assert_eq!(c.text.chars().count(), 201);
        assert!(c.text.ends_with(ELLIPSIS));
        let (line, cut) = display_line("a\nb\tc\u{202E}d\u{7}", 100);
        assert!(!cut);
        assert_eq!(line, "a↵b→c<U+202E>d\u{FFFD}");
    }

    #[test]
    fn json_on_one_line_and_booleans() {
        let a: ArrayRef = Arc::new(StringArray::from(vec![
            "{\n  \"a\": 1,\n  \"b\": [true]\n}",
        ]));
        assert_eq!(show(a, ValueKind::Json), "{ \"a\": 1, \"b\": [true] }");
        let b: ArrayRef = Arc::new(BooleanArray::from(vec![true]));
        assert_eq!(show(b, ValueKind::Boolean), "true");
    }

    #[test]
    fn unsupported_types_are_labelled() {
        let list: ArrayRef = Arc::new(ListArray::from_iter_primitive::<
            ninesql_core::driver_api::arrow_array::types::Int32Type,
            _,
            _,
        >(vec![Some(vec![Some(1)])]));
        let kind = ValueKind::of(list.data_type(), None);
        assert_eq!(kind, ValueKind::Unsupported);
        assert_eq!(show(list, kind), "<List>");
    }
}
