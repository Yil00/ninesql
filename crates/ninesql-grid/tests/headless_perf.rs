//! Bounded headless performance checks that run in CI with `cargo test` (ROADMAP P3-E). The bounds
//! are loose (debug builds, shared runners): they catch an accidental O(n) per frame or per cell,
//! not small regressions. Precise numbers come from `cargo bench -p ninesql-grid`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use ninesql_core::driver_api::arrow_array::{Int64Array, RecordBatch, StringArray};
use ninesql_core::driver_api::arrow_schema::{DataType, Field, Schema, SchemaRef};
use ninesql_core::driver_api::{ResultItem, ResultSetHeader, ResultSetKind};
use ninesql_grid::execution::header_bytes;
use ninesql_grid::model::batch_bytes;
use ninesql_grid::{
    ColumnOrder, CopyFormat, CopyOptions, DisplayOptions, ExecutionEvent, ExecutionResults,
    FetchState, GridBudget, GridSelection, ResultTab, copy_selection,
};

const ROWS: usize = 1_000_000;
const BATCH_ROWS: usize = 10_000;

fn schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::new("name", DataType::Utf8, true),
    ]))
}

fn batches() -> Vec<RecordBatch> {
    let schema = schema();
    (0..ROWS)
        .step_by(BATCH_ROWS)
        .map(|s| {
            let ids = Int64Array::from_iter_values(s as i64..(s + BATCH_ROWS) as i64);
            let names = StringArray::from_iter_values((s..s + BATCH_ROWS).map(|i| format!("n{i}")));
            RecordBatch::try_new(schema.clone(), vec![Arc::new(ids), Arc::new(names)]).unwrap()
        })
        .collect()
}

fn ingest(budget: GridBudget, batches: &[RecordBatch]) -> (ExecutionResults, Option<u32>) {
    let mut results = ExecutionResults::new(budget);
    results.apply(ExecutionEvent::Started);
    results.apply(ExecutionEvent::Item {
        statement: 0,
        item: ResultItem::ResultSet(ResultSetHeader::tabular(0, schema())),
    });
    let _ = results.request_fetch(0, true);
    let mut release = None;
    for b in batches {
        let applied = results.apply(ExecutionEvent::Item {
            statement: 0,
            item: ResultItem::Rows(b.clone()),
        });
        release = release.or(applied.release);
    }
    (results, release)
}

#[test]
fn a_million_rows_window_copy_and_memory() {
    let batches = batches();
    let total: u64 = batches.iter().map(batch_bytes).sum();

    // Memory accounting: every kept batch is counted with the stream's measure.
    let unbounded = GridBudget {
        max_bytes: u64::MAX,
        ..GridBudget::default()
    };
    let t = Instant::now();
    let (results, release) = ingest(unbounded, &batches);
    let ingest_time = t.elapsed();
    assert_eq!(release, None);
    // Plus the result set's header (its fixed overhead and schema).
    assert_eq!(
        results.bytes(),
        total + header_bytes(&ResultSetKind::Tabular(schema()))
    );
    assert!(
        ingest_time < Duration::from_secs(10),
        "ingest {ingest_time:?}"
    );
    let ResultTab::Table(model) = &results.tabs()[0] else {
        panic!("tabular");
    };
    let model = model.borrow();
    assert_eq!(model.row_count(), ROWS);

    // Visible window: cost independent of the row count.
    let options = DisplayOptions::default();
    let mut samples: Vec<Duration> = [0, 1, 499_999, 500_000, 999_950, 123_457, 765_432]
        .iter()
        .cycle()
        .take(70)
        .map(|&start| {
            let t = Instant::now();
            let w = model.window(start..start + 50, &[0, 1], &options);
            let e = t.elapsed();
            assert_eq!(w.len(), 50.min(ROWS - start));
            e
        })
        .collect();
    samples.sort();
    let p50 = samples[samples.len() / 2];
    assert!(p50 < Duration::from_millis(20), "window p50 {p50:?}");

    // Copy of a large selection, bounded.
    let t = Instant::now();
    let out = copy_selection(
        &model,
        &ColumnOrder::identity(2),
        &GridSelection::Rows {
            anchor: 0,
            focus: 99_999,
        },
        &CopyOptions::new(CopyFormat::Tsv, true),
    )
    .unwrap();
    let copy_time = t.elapsed();
    assert_eq!(out.rows, 100_000);
    assert!(!out.capped);
    assert!(copy_time < Duration::from_secs(10), "copy {copy_time:?}");
    let mut capped = CopyOptions::new(CopyFormat::Csv, false);
    capped.max_bytes = 1024 * 1024;
    let out = copy_selection(
        &model,
        &ColumnOrder::identity(2),
        &GridSelection::All,
        &capped,
    )
    .unwrap();
    assert!(out.capped && out.text.len() <= 1024 * 1024);
}

#[test]
fn the_budget_stops_ingestion_and_releases_the_stream() {
    let batches = batches();
    let total: u64 = batches.iter().map(batch_bytes).sum();
    let budget = GridBudget {
        max_bytes: total / 2,
        ..GridBudget::default()
    };
    let (results, release) = ingest(budget, &batches);
    assert_eq!(release, Some(0));
    assert!(results.bytes() <= budget.max_bytes);
    assert!(results.bytes() > budget.max_bytes - batch_bytes(&batches[0]));
    let ResultTab::Table(model) = &results.tabs()[0] else {
        panic!("tabular");
    };
    let model = model.borrow();
    assert_eq!(model.state(), FetchState::GridBudgetReached);
    assert!(model.row_count() < ROWS && model.row_count() >= ROWS / 2 - BATCH_ROWS);
}
