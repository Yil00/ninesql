//! Headless benchmarks of the results grid (ROADMAP P3-E): no GPU, no window.
//!
//! `cargo bench -p ninesql-grid` (release profile). Measures, on 1 000 000 rows of synthetic
//! Arrow data in 200-row batches (the drivers' default fetch size):
//! - ingestion of the batches into an execution's results, with memory accounting;
//! - building the visible window (50 rows x 7 columns) at random positions;
//! - copy of large selections as TSV;
//! - the cost of reading the memory account.
//!
//! The display-side numbers (time to first rows from a real server, scroll frame times) need a
//! display and a GPU: see the local protocol in the crate README.
#![allow(clippy::print_stdout, clippy::unwrap_used, clippy::expect_used)]

use std::hint::black_box;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ninesql_core::driver_api::arrow_array::{
    ArrayRef, BinaryArray, BooleanArray, Float64Array, Int64Array, RecordBatch, StringArray,
    TimestampMicrosecondArray,
};
use ninesql_core::driver_api::arrow_schema::{DataType, Field, Schema, SchemaRef, TimeUnit};
use ninesql_core::driver_api::{ENGINE_TYPE_METADATA_KEY, ResultItem, ResultSetHeader};
use ninesql_grid::{
    ColumnOrder, CopyFormat, CopyOptions, DisplayOptions, ExecutionEvent, ExecutionResults,
    GridBudget, GridSelection, ResultTab, copy_selection,
};

const ROWS: usize = 1_000_000;
const BATCH_ROWS: usize = 200;

fn schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::new("name", DataType::Utf8, true),
        Field::new("amount", DataType::Float64, true),
        Field::new(
            "created_at",
            DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())),
            true,
        ),
        Field::new("active", DataType::Boolean, true),
        Field::new("payload", DataType::Binary, true),
        Field::new("doc", DataType::Utf8, true)
            .with_metadata([(ENGINE_TYPE_METADATA_KEY.to_owned(), "jsonb".to_owned())]),
    ]))
}

fn batch(schema: &SchemaRef, start: usize, n: usize) -> RecordBatch {
    let ids: Vec<i64> = (start..start + n).map(|i| i as i64).collect();
    let cols: Vec<ArrayRef> = vec![
        Arc::new(Int64Array::from(ids.clone())),
        Arc::new(StringArray::from_iter(ids.iter().map(|i| {
            (i % 11 != 0).then(|| format!("customer {i} with a longer name"))
        }))),
        Arc::new(Float64Array::from_iter(
            ids.iter().map(|i| Some(*i as f64 * 1.25)),
        )),
        Arc::new(
            TimestampMicrosecondArray::from_iter(
                ids.iter()
                    .map(|i| Some(1_700_000_000_000_000 + i * 1_000_000)),
            )
            .with_timezone("UTC"),
        ),
        Arc::new(BooleanArray::from_iter(
            ids.iter().map(|i| Some(i % 2 == 0)),
        )),
        Arc::new(BinaryArray::from_iter(
            ids.iter().map(|i| Some(i.to_le_bytes().repeat(2))),
        )),
        Arc::new(StringArray::from_iter(ids.iter().map(|i| {
            Some(format!("{{\"id\": {i},\n \"tags\": [\"a\", \"b\"]}}"))
        }))),
    ];
    RecordBatch::try_new(schema.clone(), cols).unwrap()
}

struct Stats {
    mean: Duration,
    p50: Duration,
    p99: Duration,
    max: Duration,
}

fn stats(mut samples: Vec<Duration>) -> Stats {
    samples.sort();
    let n = samples.len();
    let sum: Duration = samples.iter().sum();
    Stats {
        mean: sum / u32::try_from(n).unwrap(),
        p50: samples[n / 2],
        p99: samples[(n * 99 / 100).min(n - 1)],
        max: samples[n - 1],
    }
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

/// Deterministic xorshift, so runs are comparable.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

fn main() {
    // `cargo bench` passes `--bench`; anything else (`cargo test --benches`) runs a quick pass.
    let full = std::env::args().any(|a| a == "--bench");
    let rows = if full { ROWS } else { 20_000 };
    let schema = schema();

    println!("ninesql-grid headless benchmarks ({rows} rows, {BATCH_ROWS}-row batches)");
    let batches: Vec<RecordBatch> = (0..rows)
        .step_by(BATCH_ROWS)
        .map(|s| batch(&schema, s, BATCH_ROWS.min(rows - s)))
        .collect();
    let arrow_bytes: u64 = batches
        .iter()
        .map(|b| b.get_array_memory_size() as u64)
        .sum();

    // 1. Ingestion and memory accounting ("fetch all" with an unbounded budget, to measure).
    let budget = GridBudget {
        max_bytes: u64::MAX,
        page_rows: BATCH_ROWS as u32,
        max_notices: GridBudget::DEFAULT_MAX_NOTICES,
    };
    let mut results = ExecutionResults::new(budget);
    let t0 = Instant::now();
    results.apply(ExecutionEvent::Started);
    results.apply(ExecutionEvent::Item {
        statement: 0,
        item: ResultItem::ResultSet(ResultSetHeader::tabular(0, schema.clone())),
    });
    let _ = results.request_fetch(0, true);
    let mut first_batch = None;
    for b in &batches {
        results.apply(ExecutionEvent::Item {
            statement: 0,
            item: ResultItem::Rows(b.clone()),
        });
        if first_batch.is_none() {
            first_batch = Some(t0.elapsed());
        }
    }
    let ingest = t0.elapsed();
    println!(
        "ingest: first batch {:?}, all {:?} ({:.0} rows/ms); accounted {:.1} MiB, Arrow {:.1} MiB",
        first_batch.unwrap_or_default(),
        ingest,
        rows as f64 / ingest.as_secs_f64() / 1000.0,
        mib(results.bytes()),
        mib(arrow_bytes),
    );
    let ResultTab::Table(model) = &results.tabs()[0] else {
        unreachable!("the header was tabular")
    };
    let model = model.borrow();
    assert_eq!(model.row_count(), rows);

    // 2. Visible window at random positions.
    let options = DisplayOptions::default();
    let cols: Vec<usize> = (0..7).collect();
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let iterations = if full { 2_000 } else { 200 };
    let mut samples = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let start = usize::try_from(rng.next()).unwrap_or(0) % (rows - 50);
        let t = Instant::now();
        black_box(model.window(start..start + 50, &cols, &options));
        samples.push(t.elapsed());
    }
    let s = stats(samples);
    println!(
        "visible window 50x7: mean {:?}, p50 {:?}, p99 {:?}, max {:?} (frame budget 16.7 ms)",
        s.mean, s.p50, s.p99, s.max
    );

    // 3. Copy of large selections (bounded by MAX_COPY_BYTES).
    let order = ColumnOrder::identity(7);
    for copy_rows in [10_000usize, 100_000, rows] {
        let copy_rows = copy_rows.min(rows);
        let selection = GridSelection::Rows {
            anchor: 0,
            focus: copy_rows - 1,
        };
        let t = Instant::now();
        let out = copy_selection(
            &model,
            &order,
            &selection,
            &CopyOptions::new(CopyFormat::Tsv, true),
        )
        .unwrap();
        println!(
            "copy {copy_rows} rows x 7 TSV: {:?}, {:.1} MiB, {} rows copied{}",
            t.elapsed(),
            mib(out.text.len() as u64),
            out.rows,
            if out.capped { " (capped)" } else { "" }
        );
    }

    // 4. Cost of reading the memory account (done for every batch and notice).
    let t = Instant::now();
    for _ in 0..10_000 {
        black_box(results.bytes());
    }
    println!("memory account read: {:?} per call", t.elapsed() / 10_000);
}
