//! Export formats: escaping per format (no injection into the output), options, and a one-million
//! row export through the export file in bounded memory (ROADMAP phase 5 exit criterion).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stderr)] // Tests may panic (AGENTS.md).

use std::sync::Arc;

use ninesql_driver_api::arrow_array::{
    ArrayRef, BinaryArray, BooleanArray, Date32Array, Float64Array, Int64Array, RecordBatch,
    StringArray, TimestampMicrosecondArray,
};
use ninesql_driver_api::arrow_schema::{DataType, Field, Schema, SchemaRef, TimeUnit};
use ninesql_driver_api::{
    AnsiDialect, ResultItem, ResultSetHeader, TRUNCATED_VALUES_METADATA_KEY, Truncation,
    TruncationReason,
};
use ninesql_transfer::{
    DialectSql, ExportFormat, ExportOptions, Exporter, LineEnding, OutputFile, Overwrite,
    QuoteStyle, SqlLiteralRenderer, TimePattern,
};

fn schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int64, true),
        Field::new("name", DataType::Utf8, true),
        Field::new("ok", DataType::Boolean, true),
        Field::new("price", DataType::Float64, true),
    ]))
}

fn batch(ids: Vec<Option<i64>>, names: Vec<Option<&str>>) -> RecordBatch {
    let n = ids.len();
    RecordBatch::try_new(
        schema(),
        vec![
            Arc::new(Int64Array::from(ids)) as ArrayRef,
            Arc::new(StringArray::from(names)),
            Arc::new(BooleanArray::from(vec![Some(true); n])),
            Arc::new(Float64Array::from(vec![Some(-1.5); n])),
        ],
    )
    .unwrap()
}

fn export(options: ExportOptions, batches: &[RecordBatch]) -> String {
    let sql: Option<Arc<dyn SqlLiteralRenderer>> =
        Some(Arc::new(DialectSql::new(Arc::new(AnsiDialect))));
    let mut ex = Exporter::new(Vec::new(), options, sql).unwrap();
    ex.write_item(&ResultItem::ResultSet(ResultSetHeader::tabular(
        0,
        schema(),
    )))
    .unwrap();
    for b in batches {
        ex.write_item(&ResultItem::Rows(b.clone())).unwrap();
    }
    let (out, stats) = ex.finish().unwrap();
    assert_eq!(stats.bytes, out.len() as u64);
    String::from_utf8(out).unwrap()
}

const NASTY: &str = "=HYPERLINK(\"http://x\")";

#[test]
fn csv_quotes_guards_formulas_and_keeps_null_distinct() {
    let b = batch(
        vec![Some(1), None, Some(3), Some(4)],
        vec![Some(NASTY), Some(""), None, Some("a,b\nc")],
    );
    let out = export(
        ExportOptions::new(ExportFormat::Csv),
        std::slice::from_ref(&b),
    );
    assert_eq!(
        out,
        "id,name,ok,price\n\
         1,\"'=HYPERLINK(\"\"http://x\"\")\",true,-1.5\n\
         ,\"\",true,-1.5\n\
         3,,true,-1.5\n\
         4,\"a,b\nc\",true,-1.5\n"
    );
    // Every guarded start character; numbers are never prefixed.
    for start in ["=", "+", "-", "@", "\t", "\r"] {
        let v = format!("{start}1+1");
        let b = batch(vec![Some(-7)], vec![Some(v.as_str())]);
        let out = export(ExportOptions::new(ExportFormat::Csv), &[b]);
        let row = out.lines().nth(1).unwrap_or_default().to_owned();
        assert!(row.starts_with("-7,"), "{row:?}");
        assert!(out.contains(&format!("'{start}1+1")), "{out:?}");
    }
    // Leading blanks and format characters do not hide a formula; full-width forms count.
    for v in [
        " =1+1",
        "\u{200B}=1",
        "\u{FEFF}@x",
        "\u{FF1D}1+1",
        "|cmd",
        "\u{3000}\u{FF0B}1",
    ] {
        assert!(ninesql_transfer::export::needs_formula_guard(v), "{v:?}");
    }
    assert!(!ninesql_transfer::export::needs_formula_guard("a=1"));
    // A NULL text the guard would flag, or holding the delimiter / quote, is refused.
    for null in ["=x", "a,b", "\""] {
        let mut o = ExportOptions::new(ExportFormat::Csv);
        o.text.null_text = null.into();
        assert!(o.validate().is_err(), "{null:?}");
    }
    let mut o = ExportOptions::new(ExportFormat::Csv);
    o.text.decimal_separator = '"';
    assert!(o.validate().is_err());
    // Guard off.
    let mut o = ExportOptions::new(ExportFormat::Csv);
    o.formula_guard = false;
    o.text.null_text = "NULL".into();
    let b = batch(vec![None], vec![Some("NULL")]);
    let out = export(o, &[b]);
    // NULL bare, the text "NULL" quoted.
    assert_eq!(out.lines().nth(1).unwrap(), "NULL,\"NULL\",true,-1.5");
}

#[test]
fn tsv_crlf_bom_always_quoted_and_separator() {
    let mut o = ExportOptions::new(ExportFormat::Tsv);
    o.bom = true;
    o.line_ending = LineEnding::CrLf;
    o.quote_style = QuoteStyle::Always;
    o.header = false;
    o.text.decimal_separator = ',';
    let out = export(o, &[batch(vec![Some(1)], vec![Some("x\ty")])]);
    assert_eq!(out, "\u{FEFF}\"1\"\t\"x\ty\"\t\"true\"\t\"-1,5\"\r\n");
    let mut o = ExportOptions::new(ExportFormat::Csv);
    o.delimiter = ';';
    o.text.decimal_separator = ',';
    let out = export(o, &[batch(vec![Some(1)], vec![Some("v")])]);
    assert_eq!(out.lines().nth(1).unwrap(), "1;v;true;-1,5");
    let mut o = ExportOptions::new(ExportFormat::Csv);
    o.text.decimal_separator = ',';
    let out = export(o, &[batch(vec![Some(1)], vec![Some("v")])]);
    assert_eq!(out.lines().nth(1).unwrap(), "1,v,true,\"-1,5\"");
}

#[test]
fn json_array_and_lines_escape() {
    let b = batch(
        vec![Some(1), None],
        vec![Some("q\"\\\n\u{1}\u{2028}</script>"), None],
    );
    let out = export(
        ExportOptions::new(ExportFormat::JsonArray),
        std::slice::from_ref(&b),
    );
    assert_eq!(
        out,
        "[\n  {\"id\": 1, \"name\": \"q\\\"\\\\\\n\\u0001\\u2028</script>\", \"ok\": true, \"price\": -1.5},\n  {\"id\": null, \"name\": null, \"ok\": true, \"price\": -1.5}\n]\n"
    );
    let out = export(ExportOptions::new(ExportFormat::JsonLines), &[b]);
    assert_eq!(out.lines().count(), 2);
    assert!(out.lines().all(|l| l.starts_with('{') && l.ends_with('}')));
    // Empty result: still a valid document.
    let ex = Exporter::new(
        Vec::new(),
        ExportOptions::new(ExportFormat::JsonArray),
        None,
    )
    .unwrap();
    assert_eq!(String::from_utf8(ex.finish().unwrap().0).unwrap(), "[]\n");
}

#[test]
fn html_escapes_and_forbids_scripts() {
    let mut o = ExportOptions::new(ExportFormat::Html);
    o.title = "<script>alert(1)</script>".into();
    let b = batch(
        vec![Some(1)],
        vec![Some("<img src=x onerror=alert(1)> & \"'")],
    );
    let out = export(o, &[b]);
    assert!(!out.contains("<script>") && !out.contains("<img"), "{out}");
    assert!(out.contains("&lt;img src=x onerror=alert(1)&gt; &amp; &quot;&#39;"));
    assert!(out.contains("Content-Security-Policy"));
    assert!(out.contains("<td class=\"null\">") || !out.contains("NULL"));
    assert!(out.trim_end().ends_with("</html>"));
}

#[test]
fn markdown_escapes_cells() {
    let b = batch(vec![Some(1), None], vec![Some("a|b\n<b>*x*</b>"), None]);
    let out = export(ExportOptions::new(ExportFormat::Markdown), &[b]);
    assert_eq!(
        out,
        "| id | name | ok | price |\n| --- | --- | --- | --- |\n\
         | 1 | a\\|b<br>\\<b\\>\\*x\\*\\<\\/b\\> | true | \\-1\\.5 |\n\
         | NULL | NULL | true | \\-1\\.5 |\n"
    );
}

#[test]
fn sql_insert_statements() {
    let mut o = ExportOptions::new(ExportFormat::SqlInsert);
    o.sql.table = vec!["s".into(), "t".into()];
    let b = batch(
        vec![Some(1), None],
        vec![Some("it's'); DROP TABLE t; --"), None],
    );
    let out = export(o.clone(), std::slice::from_ref(&b));
    assert_eq!(
        out,
        "INSERT INTO \"s\".\"t\" (\"id\", \"name\", \"ok\", \"price\") VALUES (1, 'it''s''); DROP TABLE t; --', TRUE, -1.5);\n\
         INSERT INTO \"s\".\"t\" (\"id\", \"name\", \"ok\", \"price\") VALUES (NULL, NULL, TRUE, -1.5);\n"
    );
    o.sql.rows_per_statement = 2;
    let out = export(o.clone(), &[b.clone(), b]);
    assert_eq!(out.matches("INSERT INTO").count(), 2);
    assert_eq!(out.matches(";\n").count(), 2);
    o.sql.table.clear();
    assert!(Exporter::new(Vec::new(), o, None).is_err());
}

#[test]
fn types_dates_and_patterns() {
    let schema = Arc::new(Schema::new(vec![
        Field::new("d", DataType::Date32, true),
        Field::new("ts", DataType::Timestamp(TimeUnit::Microsecond, None), true),
        Field::new(
            "tz",
            DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())),
            true,
        ),
        Field::new("b", DataType::Binary, true),
    ]));
    let b = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(Date32Array::from(vec![19_782])) as ArrayRef,
            Arc::new(TimestampMicrosecondArray::from(vec![1_500_000])),
            Arc::new(TimestampMicrosecondArray::from(vec![0]).with_timezone("UTC")),
            Arc::new(BinaryArray::from(vec![&b"\x00\xff"[..]])),
        ],
    )
    .unwrap();
    let mut o = ExportOptions::new(ExportFormat::Csv);
    o.header = false;
    o.text.date = TimePattern::parse("%d/%m/%Y").unwrap();
    let mut ex = Exporter::new(Vec::new(), o, None).unwrap();
    ex.write_item(&ResultItem::ResultSet(ResultSetHeader::tabular(0, schema)))
        .unwrap();
    ex.write_item(&ResultItem::Rows(b)).unwrap();
    let out = String::from_utf8(ex.finish().unwrap().0).unwrap();
    assert_eq!(
        out,
        "29/02/2024,1970-01-01 00:00:01.5,1970-01-01 00:00:00Z,0x00ff\n"
    );
}

#[test]
fn summary_reports_truncation_and_extra_result_sets() {
    let cut = Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int64, true),
        Field::new("name", DataType::Utf8, true)
            .with_metadata([(TRUNCATED_VALUES_METADATA_KEY.to_owned(), "true".to_owned())]),
        Field::new("ok", DataType::Boolean, true),
        Field::new("price", DataType::Float64, true),
    ]));
    let b = batch(vec![Some(1)], vec![Some("x")]);
    let b = RecordBatch::try_new(cut, b.columns().to_vec()).unwrap();
    let mut ex = Exporter::new(Vec::new(), ExportOptions::new(ExportFormat::Csv), None).unwrap();
    ex.write_item(&ResultItem::ResultSet(ResultSetHeader::tabular(
        0,
        schema(),
    )))
    .unwrap();
    ex.write_item(&ResultItem::Rows(b)).unwrap();
    ex.write_item(&ResultItem::ResultSet(ResultSetHeader::tabular(
        1,
        schema(),
    )))
    .unwrap();
    ex.write_item(&ResultItem::Rows(batch(vec![Some(2)], vec![Some("y")])))
        .unwrap();
    ex.write_item(&ResultItem::Truncated(Truncation::new(
        TruncationReason::MaxRows,
        1,
        10,
    )))
    .unwrap();
    let (out, stats) = ex.finish().unwrap();
    assert_eq!(stats.rows, 1);
    assert_eq!(stats.truncated_value_columns, vec!["name".to_owned()]);
    assert_eq!(stats.batches_with_truncated_values, 1);
    assert_eq!(stats.result_sets_skipped, 1);
    assert!(stats.incomplete.is_some());
    assert!(!String::from_utf8(out).unwrap().contains('y'));
    // Documents are refused.
    let mut ex = Exporter::new(Vec::new(), ExportOptions::new(ExportFormat::Csv), None).unwrap();
    assert!(
        ex.write_item(&ResultItem::ResultSet(ResultSetHeader::documents(0)))
            .is_err()
    );
}

/// One million rows from a generated stream, written through the export file. The exporter never
/// holds a batch, so what is buffered at any time is the batch being written, the exporter's row
/// buffer and the file's write buffer: the peak is asserted under the default per-batch budget.
#[test]
fn one_million_rows_to_csv_in_bounded_memory() {
    const ROWS: i64 = 1_000_000;
    const BATCH: i64 = 4096;
    let budget = ninesql_driver_api::FetchBudget::default().effective_max_batch_bytes();
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("million.csv");
    let file = OutputFile::create(&target, Overwrite::Refuse).unwrap();
    let mut ex = Exporter::new(file, ExportOptions::new(ExportFormat::Csv), None).unwrap();
    ex.write_item(&ResultItem::ResultSet(ResultSetHeader::tabular(
        0,
        schema(),
    )))
    .unwrap();
    let mut peak = 0u64;
    let mut next = 0i64;
    let started = std::time::Instant::now();
    while next < ROWS {
        let end = (next + BATCH).min(ROWS);
        let names: Vec<String> = (next..end).map(|i| format!("name-{i}")).collect();
        let b = batch(
            (next..end).map(Some).collect(),
            names.iter().map(|s| Some(s.as_str())).collect(),
        );
        drop(names);
        let held = b.get_array_memory_size() as u64
            + ex.buffered_bytes() as u64
            + ninesql_transfer::file::WRITE_BUFFER_BYTES as u64;
        peak = peak.max(held);
        ex.write_item(&ResultItem::Rows(b)).unwrap();
        next = end;
    }
    let (file, stats) = ex.finish().unwrap();
    let bytes = file.commit().unwrap();
    assert_eq!(stats.rows, ROWS as u64);
    assert_eq!(bytes, stats.bytes);
    let on_disk = std::fs::metadata(&target).unwrap().len();
    assert_eq!(on_disk, bytes);
    eprintln!(
        "1M-row CSV export: {bytes} bytes in {:?}, peak buffered {peak} bytes (budget {budget})",
        started.elapsed()
    );
    assert!(peak < budget, "peak {peak} >= budget {budget}");
    // The last line is the last row.
    let text = std::fs::read_to_string(&target).unwrap();
    assert!(text.ends_with("999999,name-999999,true,-1.5\n"));
    assert_eq!(text.lines().count(), ROWS as usize + 1);
}
