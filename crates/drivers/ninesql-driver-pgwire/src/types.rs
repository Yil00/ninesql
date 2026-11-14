//! PostgreSQL type → Arrow mapping and batch building under the fetch budget (ADR-0004, I6).
//!
//! | PostgreSQL | Wire format | Arrow |
//! |---|---|---|
//! | `bool` | binary | `Boolean` |
//! | `int2` / `int4` / `int8` | binary | `Int16` / `Int32` / `Int64` |
//! | `oid` | binary | `UInt32` |
//! | `float4` / `float8` | binary | `Float32` / `Float64` |
//! | `bytea` | binary | `Binary` |
//! | `date` | binary | `Date32` (±infinity → NULL, column flagged) |
//! | `time` | binary | `Time64(µs)` |
//! | `timestamp` / `timestamptz` | binary | `Timestamp(µs)` / `Timestamp(µs, "UTC")` (±infinity → NULL, flagged) |
//! | `text`, `varchar`, `bpchar`, `name`, `"char"` | text | `Utf8` |
//! | everything else (`numeric`, `interval`, `uuid`, `json`, `jsonb`, arrays, `timetz`, network, geometric, range, enum, domain…) | text | `Utf8` with `ninesql.type` |
//!
//! `numeric` is text because `NaN`, infinities and unconstrained precision have no faithful
//! Arrow decimal; arrays are their PostgreSQL text form for now.

use std::sync::Arc;

use arrow_array::builder::{
    ArrayBuilder, BinaryBuilder, BooleanBuilder, Date32Builder, Float32Builder, Float64Builder,
    Int16Builder, Int32Builder, Int64Builder, StringBuilder, Time64MicrosecondBuilder,
    TimestampMicrosecondBuilder, UInt32Builder,
};
use arrow_array::{ArrayRef, RecordBatch};
use arrow_schema::{DataType, Field, Schema, SchemaRef, TimeUnit};
use ninesql_driver_api::{
    BatchLimiter, DriverError, DriverResult, ENGINE_TYPE_METADATA_KEY, FetchBudget,
    TRUNCATED_VALUES_METADATA_KEY,
};

/// Days between 1970-01-01 and 2000-01-01 (the PostgreSQL epoch).
const PG_EPOCH_DAYS: i32 = 10_957;
/// Microseconds between 1970-01-01 and 2000-01-01.
const PG_EPOCH_MICROS: i64 = 946_684_800_000_000;

/// Decoding of one column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Bool,
    Int2,
    Int4,
    Int8,
    Oid,
    Float4,
    Float8,
    Bytea,
    Date,
    Time,
    Timestamp,
    TimestampTz,
    /// Text format, `Utf8`.
    Text,
}

impl Kind {
    /// Wire size of a binary fixed-width value; `None` for variable-length kinds.
    pub(crate) fn fixed_len(self) -> Option<usize> {
        match self {
            Self::Bool => Some(1),
            Self::Int2 => Some(2),
            Self::Int4 | Self::Oid | Self::Float4 | Self::Date => Some(4),
            Self::Int8 | Self::Float8 | Self::Time | Self::Timestamp | Self::TimestampTz => Some(8),
            Self::Bytea | Self::Text => None,
        }
    }

    /// Result format code requested in `Bind`: 1 = binary, 0 = text.
    pub(crate) fn format(self) -> i16 {
        i16::from(self != Self::Text)
    }

    fn data_type(self) -> DataType {
        match self {
            Self::Bool => DataType::Boolean,
            Self::Int2 => DataType::Int16,
            Self::Int4 => DataType::Int32,
            Self::Int8 => DataType::Int64,
            Self::Oid => DataType::UInt32,
            Self::Float4 => DataType::Float32,
            Self::Float8 => DataType::Float64,
            Self::Bytea => DataType::Binary,
            Self::Date => DataType::Date32,
            Self::Time => DataType::Time64(TimeUnit::Microsecond),
            Self::Timestamp => DataType::Timestamp(TimeUnit::Microsecond, None),
            Self::TimestampTz => DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())),
            Self::Text => DataType::Utf8,
        }
    }
}

/// Built-in type names for OIDs the driver knows; other names are looked up in `pg_type`.
pub(crate) fn builtin_type_name(oid: u32) -> Option<&'static str> {
    Some(match oid {
        16 => "boolean",
        17 => "bytea",
        18 => "\"char\"",
        19 => "name",
        20 => "bigint",
        21 => "smallint",
        23 => "integer",
        25 => "text",
        26 => "oid",
        114 => "json",
        142 => "xml",
        650 => "cidr",
        700 => "real",
        701 => "double precision",
        774 => "macaddr8",
        790 => "money",
        829 => "macaddr",
        869 => "inet",
        1000 => "boolean[]",
        1005 => "smallint[]",
        1007 => "integer[]",
        1009 => "text[]",
        1015 => "character varying[]",
        1016 => "bigint[]",
        1021 => "real[]",
        1022 => "double precision[]",
        1042 => "character",
        1043 => "character varying",
        1082 => "date",
        1083 => "time without time zone",
        1114 => "timestamp without time zone",
        1184 => "timestamp with time zone",
        1186 => "interval",
        1266 => "time with time zone",
        1560 => "bit",
        1562 => "bit varying",
        1700 => "numeric",
        2249 => "record",
        2278 => "void",
        2950 => "uuid",
        3802 => "jsonb",
        3614 => "tsvector",
        3904 => "int4range",
        600 => "point",
        _ => return None,
    })
}

/// How a column is decoded and presented.
#[derive(Clone, Debug)]
pub(crate) struct ColPlan {
    pub(crate) name: String,
    pub(crate) oid: u32,
    pub(crate) kind: Kind,
}

impl ColPlan {
    pub(crate) fn new(name: String, oid: u32) -> Self {
        let kind = match oid {
            16 => Kind::Bool,
            21 => Kind::Int2,
            23 => Kind::Int4,
            20 => Kind::Int8,
            26 => Kind::Oid,
            700 => Kind::Float4,
            701 => Kind::Float8,
            17 => Kind::Bytea,
            1082 => Kind::Date,
            1083 => Kind::Time,
            1114 => Kind::Timestamp,
            1184 => Kind::TimestampTz,
            _ => Kind::Text,
        };
        Self { name, oid, kind }
    }

    /// `true` when `Utf8` is not a faithful mapping (the field then carries `ninesql.type`).
    fn needs_type_metadata(&self) -> bool {
        self.kind == Kind::Text && !matches!(self.oid, 18 | 19 | 25 | 1042 | 1043)
    }
}

/// The Arrow schema of a result (fields are nullable: PostgreSQL does not report nullability).
pub(crate) fn schema_for(plans: &[ColPlan], type_name: &dyn Fn(u32) -> String) -> SchemaRef {
    let fields: Vec<Field> = plans
        .iter()
        .map(|p| {
            let f = Field::new(p.name.clone(), p.kind.data_type(), true);
            if p.needs_type_metadata() {
                f.with_metadata([(ENGINE_TYPE_METADATA_KEY.to_owned(), type_name(p.oid))])
            } else {
                f
            }
        })
        .collect();
    Arc::new(Schema::new(fields))
}

enum ColBuilder {
    Bool(BooleanBuilder),
    I16(Int16Builder),
    I32(Int32Builder),
    I64(Int64Builder),
    U32(UInt32Builder),
    F32(Float32Builder),
    F64(Float64Builder),
    Binary(BinaryBuilder),
    Date(Date32Builder),
    Time(Time64MicrosecondBuilder),
    Ts(TimestampMicrosecondBuilder),
    TsTz(TimestampMicrosecondBuilder),
    Utf8(StringBuilder),
}

impl ColBuilder {
    fn new(kind: Kind) -> Self {
        match kind {
            Kind::Bool => Self::Bool(BooleanBuilder::new()),
            Kind::Int2 => Self::I16(Int16Builder::new()),
            Kind::Int4 => Self::I32(Int32Builder::new()),
            Kind::Int8 => Self::I64(Int64Builder::new()),
            Kind::Oid => Self::U32(UInt32Builder::new()),
            Kind::Float4 => Self::F32(Float32Builder::new()),
            Kind::Float8 => Self::F64(Float64Builder::new()),
            Kind::Bytea => Self::Binary(BinaryBuilder::new()),
            Kind::Date => Self::Date(Date32Builder::new()),
            Kind::Time => Self::Time(Time64MicrosecondBuilder::new()),
            Kind::Timestamp => Self::Ts(TimestampMicrosecondBuilder::new()),
            Kind::TimestampTz => {
                Self::TsTz(TimestampMicrosecondBuilder::new().with_timezone("UTC"))
            }
            Kind::Text => Self::Utf8(StringBuilder::new()),
        }
    }

    fn append_null(&mut self) {
        match self {
            Self::Bool(b) => b.append_null(),
            Self::I16(b) => b.append_null(),
            Self::I32(b) => b.append_null(),
            Self::I64(b) => b.append_null(),
            Self::U32(b) => b.append_null(),
            Self::F32(b) => b.append_null(),
            Self::F64(b) => b.append_null(),
            Self::Binary(b) => b.append_null(),
            Self::Date(b) => b.append_null(),
            Self::Time(b) => b.append_null(),
            Self::Ts(b) | Self::TsTz(b) => b.append_null(),
            Self::Utf8(b) => b.append_null(),
        }
    }

    fn finish(&mut self) -> ArrayRef {
        match self {
            Self::Bool(b) => Arc::new(b.finish()),
            Self::I16(b) => Arc::new(b.finish()),
            Self::I32(b) => Arc::new(b.finish()),
            Self::I64(b) => Arc::new(b.finish()),
            Self::U32(b) => Arc::new(b.finish()),
            Self::F32(b) => Arc::new(b.finish()),
            Self::F64(b) => Arc::new(b.finish()),
            Self::Binary(b) => Arc::new(b.finish()),
            Self::Date(b) => Arc::new(b.finish()),
            Self::Time(b) => Arc::new(b.finish()),
            Self::Ts(b) | Self::TsTz(b) => Arc::new(b.finish()),
            Self::Utf8(b) => Arc::new(b.finish()),
        }
    }

    fn len(&self) -> usize {
        match self {
            Self::Bool(b) => b.len(),
            Self::I16(b) => b.len(),
            Self::I32(b) => b.len(),
            Self::I64(b) => b.len(),
            Self::U32(b) => b.len(),
            Self::F32(b) => b.len(),
            Self::F64(b) => b.len(),
            Self::Binary(b) => b.len(),
            Self::Date(b) => b.len(),
            Self::Time(b) => b.len(),
            Self::Ts(b) | Self::TsTz(b) => b.len(),
            Self::Utf8(b) => b.len(),
        }
    }
}

fn be<const N: usize>(v: &[u8]) -> DriverResult<[u8; N]> {
    v.try_into()
        .map_err(|_| DriverError::Protocol(format!("expected a {N}-byte binary value")))
}

/// Builds the batches of one result set. Rows are appended value by value by the connection's
/// `DataRow` reader, which already cut values to the row budget.
pub(crate) struct BatchBuilder {
    schema: SchemaRef,
    plans: Arc<[ColPlan]>,
    cols: Vec<ColBuilder>,
    truncated: Vec<bool>,
    budget: FetchBudget,
    limiter: BatchLimiter,
    /// Columns where an infinite date / timestamp was replaced by NULL (reported once).
    infinite: Vec<bool>,
    infinite_reported: Vec<bool>,
}

impl BatchBuilder {
    pub(crate) fn new(schema: SchemaRef, plans: Arc<[ColPlan]>, budget: FetchBudget) -> Self {
        let cols = plans.iter().map(|p| ColBuilder::new(p.kind)).collect();
        let n = plans.len();
        Self {
            schema,
            plans,
            cols,
            truncated: vec![false; n],
            limiter: BatchLimiter::new(&budget),
            budget,
            infinite: vec![false; n],
            infinite_reported: vec![false; n],
        }
    }

    pub(crate) fn plans(&self) -> &[ColPlan] {
        &self.plans
    }

    pub(crate) fn budget(&self) -> &FetchBudget {
        &self.budget
    }

    pub(crate) fn rows(&self) -> u32 {
        self.limiter.rows()
    }

    pub(crate) fn is_full(&self) -> bool {
        self.limiter.is_full()
    }

    /// Records the end of a row of `bytes` estimated bytes.
    pub(crate) fn end_row(&mut self, bytes: u64) {
        self.limiter.add_row(bytes);
    }

    pub(crate) fn null(&mut self, col: usize) {
        self.cols[col].append_null();
    }

    /// Appends one non-null value (already cut when `cut`).
    pub(crate) fn value(&mut self, col: usize, v: &[u8], cut: bool) -> DriverResult<()> {
        if cut {
            self.truncated[col] = true;
        }
        match &mut self.cols[col] {
            ColBuilder::Bool(b) => b.append_value(be::<1>(v)?[0] != 0),
            ColBuilder::I16(b) => b.append_value(i16::from_be_bytes(be(v)?)),
            ColBuilder::I32(b) => b.append_value(i32::from_be_bytes(be(v)?)),
            ColBuilder::I64(b) => b.append_value(i64::from_be_bytes(be(v)?)),
            ColBuilder::U32(b) => b.append_value(u32::from_be_bytes(be(v)?)),
            ColBuilder::F32(b) => b.append_value(f32::from_be_bytes(be(v)?)),
            ColBuilder::F64(b) => b.append_value(f64::from_be_bytes(be(v)?)),
            ColBuilder::Binary(b) => b.append_value(v),
            ColBuilder::Date(b) => {
                let raw = i32::from_be_bytes(be(v)?);
                // ±infinity are i32::MAX / i32::MIN on the wire.
                match raw.checked_add(PG_EPOCH_DAYS) {
                    Some(d) if raw != i32::MAX && raw != i32::MIN => b.append_value(d),
                    _ => {
                        b.append_null();
                        self.infinite[col] = true;
                        self.truncated[col] = true;
                    }
                }
            }
            ColBuilder::Time(b) => b.append_value(i64::from_be_bytes(be(v)?)),
            ColBuilder::Ts(b) | ColBuilder::TsTz(b) => {
                let raw = i64::from_be_bytes(be(v)?);
                if raw == i64::MAX || raw == i64::MIN {
                    b.append_null();
                    self.infinite[col] = true;
                    self.truncated[col] = true;
                } else {
                    match raw.checked_add(PG_EPOCH_MICROS) {
                        Some(t) => b.append_value(t),
                        None => {
                            b.append_null();
                            self.truncated[col] = true;
                        }
                    }
                }
            }
            ColBuilder::Utf8(b) => match std::str::from_utf8(v) {
                Ok(s) => b.append_value(s),
                Err(e) if e.error_len().is_none() => {
                    // A cut in the middle of a multi-byte character: drop the partial one.
                    let s = std::str::from_utf8(&v[..e.valid_up_to()]).unwrap_or("");
                    b.append_value(s);
                    self.truncated[col] = true;
                }
                Err(_) => b.append_value(String::from_utf8_lossy(v)),
            },
        }
        Ok(())
    }

    /// Columns whose infinite values were mapped to NULL since the last call, by name.
    pub(crate) fn take_new_infinite_columns(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        for (i, inf) in self.infinite.iter().enumerate() {
            if *inf && !self.infinite_reported[i] {
                self.infinite_reported[i] = true;
                out.push(self.plans[i].name.clone());
            }
        }
        out
    }

    /// Emits the rows built so far as one batch (fields with cut values flagged) and starts the
    /// next batch. `None` when no row was built.
    pub(crate) fn finish(&mut self) -> DriverResult<Option<RecordBatch>> {
        let n = self.cols.first().map_or(0, ColBuilder::len);
        if n == 0 && self.limiter.rows() == 0 {
            return Ok(None);
        }
        let schema = if self.truncated.iter().any(|t| *t) {
            let fields: Vec<Field> = self
                .schema
                .fields()
                .iter()
                .zip(&self.truncated)
                .map(|(f, t)| {
                    if *t {
                        let mut meta = f.metadata().clone();
                        meta.insert(TRUNCATED_VALUES_METADATA_KEY.to_owned(), "true".to_owned());
                        f.as_ref().clone().with_metadata(meta)
                    } else {
                        f.as_ref().clone()
                    }
                })
                .collect();
            Arc::new(Schema::new(fields))
        } else {
            Arc::clone(&self.schema)
        };
        let columns: Vec<ArrayRef> = self.cols.iter_mut().map(ColBuilder::finish).collect();
        let rows = self.limiter.rows() as usize;
        self.limiter.reset();
        self.truncated.iter_mut().for_each(|t| *t = false);
        self.infinite.iter_mut().for_each(|t| *t = false);
        let options = arrow_array::RecordBatchOptions::new().with_row_count(Some(rows));
        RecordBatch::try_new_with_options(schema, columns, &options)
            .map(Some)
            .map_err(|e| DriverError::Protocol(format!("cannot build a batch: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::Array;
    use arrow_array::cast::AsArray;
    use arrow_array::types::{Date32Type, Int32Type, TimestampMicrosecondType};

    fn builder(oids: &[u32]) -> BatchBuilder {
        let plans: Arc<[ColPlan]> = oids
            .iter()
            .enumerate()
            .map(|(i, o)| ColPlan::new(format!("c{i}"), *o))
            .collect();
        let schema = schema_for(&plans, &|o| format!("oid{o}"));
        BatchBuilder::new(schema, plans, FetchBudget::default())
    }

    #[test]
    fn decodes_binary_values_and_flags() {
        let mut b = builder(&[23, 1082, 1184, 25, 1700]);
        b.value(0, &42i32.to_be_bytes(), false).unwrap();
        b.value(1, &0i32.to_be_bytes(), false).unwrap();
        b.value(2, &i64::MAX.to_be_bytes(), false).unwrap();
        b.value(3, "é".as_bytes()[..1].as_ref(), true).unwrap();
        b.value(4, b"12.50", false).unwrap();
        b.end_row(10);
        let batch = b.finish().unwrap().unwrap();
        assert_eq!(batch.num_rows(), 1);
        assert_eq!(batch.column(0).as_primitive::<Int32Type>().value(0), 42);
        assert_eq!(
            batch.column(1).as_primitive::<Date32Type>().value(0),
            PG_EPOCH_DAYS
        );
        assert!(
            batch
                .column(2)
                .as_primitive::<TimestampMicrosecondType>()
                .is_null(0)
        );
        assert_eq!(batch.column(3).as_string::<i32>().value(0), "");
        let schema = batch.schema();
        assert!(
            schema
                .field(2)
                .metadata()
                .contains_key(TRUNCATED_VALUES_METADATA_KEY)
        );
        assert!(
            schema
                .field(3)
                .metadata()
                .contains_key(TRUNCATED_VALUES_METADATA_KEY)
        );
        assert_eq!(
            schema
                .field(4)
                .metadata()
                .get(ENGINE_TYPE_METADATA_KEY)
                .map(String::as_str),
            Some("oid1700")
        );
        assert!(
            !schema
                .field(3)
                .metadata()
                .contains_key(ENGINE_TYPE_METADATA_KEY)
        );
        assert_eq!(b.take_new_infinite_columns(), Vec::<String>::new());
        // The next batch starts clean.
        b.value(0, &1i32.to_be_bytes(), false).unwrap();
        b.null(1);
        b.null(2);
        b.null(3);
        b.null(4);
        b.end_row(4);
        let next = b.finish().unwrap().unwrap();
        assert!(next.schema().field(3).metadata().is_empty());
        assert!(b.finish().unwrap().is_none());
    }

    #[test]
    fn wrong_binary_width_is_a_protocol_error() {
        let mut b = builder(&[20]);
        assert!(matches!(
            b.value(0, &[1, 2, 3], false),
            Err(DriverError::Protocol(_))
        ));
    }
}
