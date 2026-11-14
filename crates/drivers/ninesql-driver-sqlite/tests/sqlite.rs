//! Integration tests of `ninesql-driver-sqlite` against temporary database files and the
//! "nine" dataset of `dev/seed/sqlite/nine.sql` (file engine: no container, never skipped).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use arrow_array::Array;
use arrow_array::cast::AsArray;
use arrow_array::types::{Float64Type, Int64Type};
use arrow_schema::DataType;
use ninesql_driver_api::conformance;
use ninesql_driver_api::{
    CancelToken, ConnectParams, DriverError, Endpoint, EngineId, ExecOptions, FetchBudget,
    ObjectKind, ObjectPath, PageRequest, Param, PropertyValue, ResultItem, ResultSetKind,
    RowChange, RowEdit, Session, Statement, TRUNCATED_VALUES_METADATA_KEY,
};
use ninesql_driver_api::{Driver, ENGINE_TYPE_METADATA_KEY};
use ninesql_driver_sqlite::SqliteDriver;

/// The seeded "nine" database, built once per test run under Cargo's target tmp directory.
fn nine_db() -> &'static Path {
    static DB: OnceLock<PathBuf> = OnceLock::new();
    DB.get_or_init(|| {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("ninesql-sqlite");
        std::fs::create_dir_all(&dir).unwrap();
        let seed_path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../dev/seed/sqlite/nine.sql");
        let seed = std::fs::read_to_string(&seed_path).unwrap();
        let db = dir.join("nine-v1.db");
        if !db.exists() {
            let tmp = dir.join(format!("nine-{}.tmp", std::process::id()));
            let _ = std::fs::remove_file(&tmp);
            let c = rusqlite::Connection::open(&tmp).unwrap();
            c.execute_batch(&seed).unwrap();
            drop(c);
            std::fs::rename(&tmp, &db).unwrap();
        }
        db
    })
}

/// A private copy of the "nine" database (for tests that write or check the file).
fn nine_copy(dir: &Path) -> PathBuf {
    let p = dir.join("nine.db");
    std::fs::copy(nine_db(), &p).unwrap();
    p
}

fn params(path: &Path) -> ConnectParams {
    ConnectParams::new(EngineId::SQLITE, Endpoint::file(path))
}

fn ro_params(path: &Path) -> ConnectParams {
    let mut p = params(path);
    p.read_only = true;
    p
}

async fn connect(p: ConnectParams) -> Box<dyn Session> {
    match SqliteDriver::new().connect(p).await {
        Ok(s) => s,
        Err(e) => panic!("connect failed: {e}"),
    }
}

struct Outcome {
    items: Vec<ResultItem>,
    rows: u64,
}

async fn run_opts(
    s: &mut dyn Session,
    st: Statement,
    opts: ExecOptions,
) -> Result<Outcome, DriverError> {
    // A stream dropped by the previous call (budget reached, error) frees the session
    // asynchronously: wait for it like the core does.
    let start = Instant::now();
    let mut stream = loop {
        match s.execute(st.clone(), opts.clone()).await {
            Err(DriverError::Busy) if start.elapsed() < Duration::from_secs(5) => {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            other => break other?,
        }
    };
    let mut items = Vec::new();
    let mut rows = 0;
    while let Some(item) = stream.next().await {
        let item = item?;
        if let ResultItem::Rows(b) = &item {
            rows += b.num_rows() as u64;
        }
        items.push(item);
    }
    Ok(Outcome { items, rows })
}

async fn run(s: &mut dyn Session, sql: &str) -> Result<Outcome, DriverError> {
    run_opts(s, Statement::sql(sql), ExecOptions::default()).await
}

/// Executes once the session is free again (a dropped stream frees it asynchronously).
async fn run_when_free(s: &mut dyn Session, sql: &str) -> Result<Outcome, DriverError> {
    let start = Instant::now();
    loop {
        match run(s, sql).await {
            Err(DriverError::Busy) if start.elapsed() < Duration::from_secs(5) => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            other => return other,
        }
    }
}

fn update_count(o: &Outcome) -> Option<Option<u64>> {
    o.items.iter().find_map(|i| match i {
        ResultItem::UpdateCount(n) => Some(*n),
        _ => None,
    })
}

fn first_i64(o: &Outcome) -> Option<i64> {
    o.items.iter().find_map(|i| match i {
        ResultItem::Rows(b) if b.num_rows() > 0 => {
            let c = b.column(0);
            c.as_primitive_opt::<Int64Type>().map(|a| a.value(0))
        }
        _ => None,
    })
}

fn files_in(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut out: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| {
            let e = e.unwrap();
            (
                e.file_name().to_string_lossy().into_owned(),
                std::fs::read(e.path()).unwrap_or_default(),
            )
        })
        .collect();
    out.sort();
    out
}

const LONG_RUNNING: &str =
    "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c) SELECT count(*) FROM c";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conformance_helpers() {
    let d = SqliteDriver::new();
    let db = nine_db();
    conformance::assert_reserved_properties_rejected(&d, &params(db)).await;
    conformance::assert_multiple_statements_refused(&d, ro_params(db)).await;
    conformance::assert_identifier_quoting(
        &d,
        ro_params(db),
        conformance::ALIAS_TEMPLATE,
        conformance::IDENTIFIER_SAMPLES,
    )
    .await;
    let rows = conformance::assert_stale_cancel_ignored(
        &d,
        ro_params(db),
        "SELECT 1",
        "SELECT * FROM big_events LIMIT 300000",
    )
    .await;
    assert_eq!(rows, 300_000);
    let budget = FetchBudget::default()
        .with_batch_rows(700)
        .with_max_value_bytes(1000)
        .with_max_row_bytes(2500)
        .with_max_batch_bytes(256 * 1024);
    let report = conformance::assert_batches_within_budget(
        &d,
        ro_params(db),
        "SELECT id, printf('%.*c', 3000, 'x') AS wide, payload, zeroblob(2000) AS b, \
         kind || printf('%.*c', 900, 'y') AS k FROM big_events LIMIT 20000",
        budget,
    )
    .await;
    assert!(report.truncated_values);
    assert_eq!(report.rows, 20_000);
    assert!(report.batches >= 20_000 / 700);
    // `assert_access_mode_change_detected` needs a liftable mechanism; SQLite's (`OpenMode`)
    // is not: `PRAGMA query_only = OFF` is refused client-side and denied engine-side
    // (`read_only_refuses_every_bypass`). `assert_verify_full_rejects_untrusted_certificate`
    // does not apply to a file engine (no TLS).
    assert!(
        !d.capabilities(&EngineId::SQLITE, None)
            .unwrap()
            .read_only
            .is_liftable()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_only_refuses_every_bypass() {
    let dir = tempfile::tempdir().unwrap();
    let db = nine_copy(dir.path());
    // A trigger that would write if anything reached the table.
    {
        let c = rusqlite::Connection::open(&db).unwrap();
        c.execute_batch(
            "CREATE TABLE ninesql_conformance (a INTEGER); \
             CREATE TABLE audit (n INTEGER); \
             CREATE TRIGGER nine_meta_ai AFTER INSERT ON nine_meta \
             BEGIN INSERT INTO audit VALUES (1); END;",
        )
        .unwrap();
    }
    let before = files_in(dir.path());
    let d = SqliteDriver::new();
    let writes = conformance::writes_for(&EngineId::SQLITE);
    conformance::assert_read_only_refuses_writes(&d, ro_params(&db), &writes).await;
    let target = dir.path().join("vacuum-copy.db");
    let attached = dir.path().join("attached.db");
    let extra = [
        format!("VACUUM INTO '{}'", target.display()),
        "VACUUM".to_owned(),
        format!("ATTACH DATABASE '{}' AS other", attached.display()),
        format!("ATTACH 'file:{}?mode=rwc' AS other", attached.display()),
        "DETACH DATABASE main".to_owned(),
        "SELECT load_extension('/nonexistent/evil.so')".to_owned(),
        "CREATE TEMP TABLE scratch (a)".to_owned(),
        "CREATE TEMPORARY VIEW tv AS SELECT 1".to_owned(),
        "PRAGMA temp_store = FILE".to_owned(),
        "PRAGMA writable_schema = 1".to_owned(),
        "PRAGMA [query_only] = 0".to_owned(),
        "PRAGMA main.query_only = false".to_owned(),
        "PRAGMA journal_mode = WAL".to_owned(),
        "PRAGMA user_version = 3".to_owned(),
        "INSERT INTO nine_meta VALUES ('k', 'v')".to_owned(),
        "WITH x AS (SELECT 1) DELETE FROM orders".to_owned(),
        "REPLACE INTO nine_meta VALUES ('seed_version', '2')".to_owned(),
        "ANALYZE".to_owned(),
        "REINDEX".to_owned(),
        "BEGIN IMMEDIATE".to_owned(),
    ];
    let mut s = connect(ro_params(&db)).await;
    for sql in &extra {
        let r = run(s.as_mut(), sql).await;
        assert!(
            matches!(r, Err(DriverError::ReadOnly(_))),
            "{sql} -> {:?}",
            r.map(|o| o.rows)
        );
    }
    // Init statements run under the read-only mode too.
    let mut p = ro_params(&db);
    p.init_statements = vec!["DELETE FROM nine_meta".to_owned()];
    assert!(matches!(d.connect(p).await, Err(DriverError::ReadOnly(_))));
    // Still read-only, still usable: reads, schema pragmas, read transactions.
    assert!(s.is_read_only());
    for sql in [
        "SELECT count(*) FROM customers",
        "PRAGMA table_info(customers)",
        "PRAGMA query_only",
        "SELECT * FROM customer_order_totals LIMIT 3",
    ] {
        run(s.as_mut(), sql)
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    s.set_auto_commit(false).await.unwrap();
    let o = run(s.as_mut(), "SELECT count(*) FROM orders")
        .await
        .unwrap();
    assert_eq!(first_i64(&o), Some(10_000));
    assert!(s.in_transaction());
    s.commit().await.unwrap();
    assert!(!s.in_transaction());
    assert!(matches!(
        s.set_read_only(false).await,
        Err(DriverError::Unsupported(_))
    ));
    s.close().await.unwrap();
    // I4: no file changed, none appeared (no journal, no VACUUM INTO target, no attached file).
    assert_eq!(files_in(dir.path()), before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_and_timeout_interrupt_the_statement() {
    let mut s = connect(ro_params(nine_db())).await;
    // Cancel through the stream's handle while the statement runs.
    let mut stream = s
        .execute(Statement::sql(LONG_RUNNING), ExecOptions::default())
        .await
        .unwrap();
    let handle = stream.cancel_handle();
    let started = Instant::now();
    let canceller = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        handle.cancel().await
    });
    let mut outcome = None;
    while let Some(item) = stream.next().await {
        if let Err(e) = item {
            outcome = Some(e);
            break;
        }
    }
    canceller.await.unwrap().unwrap();
    assert!(
        matches!(outcome, Some(DriverError::Cancelled)),
        "{outcome:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(5));
    drop(stream);
    assert_eq!(
        first_i64(&run_when_free(s.as_mut(), "SELECT 41 + 1").await.unwrap()),
        Some(42)
    );
    // The user's cancel token.
    let token = CancelToken::new();
    let t = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        t.cancel();
    });
    let r = run_opts(
        s.as_mut(),
        Statement::sql(LONG_RUNNING),
        ExecOptions::default().with_cancel(token),
    )
    .await;
    assert!(matches!(r, Err(DriverError::Cancelled)), "{:?}", r.err());
    // Timeout.
    let started = Instant::now();
    let r = run_opts(
        s.as_mut(),
        Statement::sql(LONG_RUNNING),
        ExecOptions::default().with_timeout(Some(Duration::from_millis(300))),
    )
    .await;
    assert!(matches!(r, Err(DriverError::Timeout(_))), "{:?}", r.err());
    assert!(started.elapsed() < Duration::from_secs(6));
    assert_eq!(
        first_i64(&run_when_free(s.as_mut(), "SELECT 7").await.unwrap()),
        Some(7)
    );
    // The session-level handle does nothing when idle.
    s.cancel().await.unwrap();
    assert_eq!(
        first_i64(&run(s.as_mut(), "SELECT 8").await.unwrap()),
        Some(8)
    );
}

/// A paused result holds a read lock; cancelling (or dropping) it releases the lock, and a
/// timeout fires even while the consumer is not polling.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn paused_results_release_their_lock() {
    let dir = tempfile::tempdir().unwrap();
    let db = nine_copy(dir.path());
    let mut s = connect(ro_params(&db)).await;
    let writer = rusqlite::Connection::open(&db).unwrap();
    writer.busy_timeout(Duration::from_millis(100)).unwrap();
    let mut stream = s
        .execute(
            Statement::sql("SELECT * FROM big_events"),
            ExecOptions::default(),
        )
        .await
        .unwrap();
    // Header + first batch, then pause.
    let mut got_rows = false;
    while !got_rows {
        got_rows = matches!(stream.next().await, Some(Ok(ResultItem::Rows(_))));
    }
    assert!(
        writer
            .execute(
                "UPDATE nine_meta SET value = 'x' WHERE key = 'seed_version'",
                []
            )
            .is_err(),
        "the paused reader should hold its SHARED lock"
    );
    stream.cancel_handle().cancel().await.unwrap();
    drop(stream);
    run_when_free(s.as_mut(), "SELECT 1").await.unwrap();
    writer
        .execute(
            "UPDATE nine_meta SET value = 'x' WHERE key = 'seed_version'",
            [],
        )
        .unwrap();
    // Dropped mid-result: the session is free again, the lock released.
    let mut stream = s
        .execute(
            Statement::sql("SELECT * FROM big_events"),
            ExecOptions::default(),
        )
        .await
        .unwrap();
    let _ = stream.next().await;
    let _ = stream.next().await;
    drop(stream);
    run_when_free(s.as_mut(), "SELECT 1").await.unwrap();
    writer
        .execute(
            "UPDATE nine_meta SET value = 'y' WHERE key = 'seed_version'",
            [],
        )
        .unwrap();
    // A timeout while nobody polls: the watchdog interrupts, the next poll reports it.
    let mut stream = s
        .execute(
            Statement::sql("SELECT * FROM big_events"),
            ExecOptions::default().with_timeout(Some(Duration::from_millis(300))),
        )
        .await
        .unwrap();
    let _ = stream.next().await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    let mut timed_out = false;
    while let Some(item) = stream.next().await {
        if matches!(item, Err(DriverError::Timeout(_))) {
            timed_out = true;
            break;
        }
    }
    assert!(timed_out);
    drop(stream);
    run_when_free(s.as_mut(), "SELECT 1").await.unwrap();
    // Close while a result is paused returns promptly.
    let mut stream = s
        .execute(
            Statement::sql("SELECT * FROM big_events"),
            ExecOptions::default(),
        )
        .await
        .unwrap();
    let _ = stream.next().await;
    let started = Instant::now();
    s.close().await.unwrap();
    assert!(started.elapsed() < Duration::from_secs(5));
    let mut closed = false;
    while let Some(item) = stream.next().await {
        if let Err(e) = item {
            closed = matches!(e, DriverError::Closed(_));
            break;
        }
    }
    assert!(closed, "the paused stream should report the closed session");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn budgets_truncate_and_bound_values() {
    let mut s = connect(ro_params(nine_db())).await;
    // Row budget.
    let o = run_opts(
        s.as_mut(),
        Statement::sql("SELECT * FROM big_events"),
        ExecOptions::default().with_budget(FetchBudget::default().with_max_rows(Some(1234))),
    )
    .await
    .unwrap();
    assert_eq!(o.rows, 1234);
    assert!(matches!(o.items.last(), Some(ResultItem::Truncated(_))));
    // Byte budget.
    let o = run_opts(
        s.as_mut(),
        Statement::sql("SELECT * FROM big_events"),
        ExecOptions::default().with_budget(FetchBudget::default().with_max_bytes(512 * 1024)),
    )
    .await
    .unwrap();
    assert!(o.rows > 0 && o.rows < 1_000_000);
    assert!(matches!(o.items.last(), Some(ResultItem::Truncated(_))));
    // A value cut while copied, the column flagged.
    let o = run_opts(
        s.as_mut(),
        Statement::sql("SELECT printf('%.*c', 5000, 'z') AS v"),
        ExecOptions::default().with_budget(FetchBudget::default().with_max_value_bytes(100)),
    )
    .await
    .unwrap();
    let batch = o
        .items
        .iter()
        .find_map(|i| match i {
            ResultItem::Rows(b) => Some(b.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(batch.column(0).as_string::<i32>().value(0).len(), 100);
    assert_eq!(
        batch
            .schema()
            .field(0)
            .metadata()
            .get(TRUNCATED_VALUES_METADATA_KEY)
            .map(String::as_str),
        Some("true")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn types_map_to_arrow() {
    let mut s = connect(ro_params(nine_db())).await;
    let o = run(s.as_mut(), "SELECT * FROM all_types ORDER BY id")
        .await
        .unwrap();
    let schema = o
        .items
        .iter()
        .find_map(|i| match i {
            ResultItem::ResultSet(h) => match &h.kind {
                ResultSetKind::Tabular(s) => Some(s.clone()),
                ResultSetKind::Documents => None,
            },
            _ => None,
        })
        .unwrap();
    let ty = |name: &str| schema.field_with_name(name).unwrap().data_type().clone();
    let engine_type = |name: &str| {
        schema
            .field_with_name(name)
            .unwrap()
            .metadata()
            .get(ENGINE_TYPE_METADATA_KEY)
            .cloned()
    };
    assert_eq!(ty("c_integer"), DataType::Int64);
    assert_eq!(ty("c_bigint"), DataType::Int64);
    assert_eq!(ty("c_real"), DataType::Float64);
    assert_eq!(ty("c_double"), DataType::Float64);
    assert_eq!(ty("c_boolean"), DataType::Boolean);
    assert_eq!(ty("c_text"), DataType::Utf8);
    assert_eq!(ty("c_varchar"), DataType::Utf8);
    assert_eq!(ty("c_blob"), DataType::Binary);
    for (c, t) in [
        ("c_numeric", "NUMERIC"),
        ("c_decimal", "DECIMAL(20,6)"),
        ("c_date", "DATE"),
        ("c_datetime", "DATETIME"),
        ("c_json", "JSON"),
    ] {
        assert_eq!(ty(c), DataType::Utf8, "{c}");
        assert_eq!(engine_type(c).as_deref(), Some(t), "{c}");
    }
    // Untyped column: 'any' then 42 → text, the converted value flagged, one notice.
    assert_eq!(ty("c_untyped"), DataType::Utf8);
    let batch = o
        .items
        .iter()
        .find_map(|i| match i {
            ResultItem::Rows(b) => Some(b.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(batch.num_rows(), 3);
    let col = |n: &str| batch.column(batch.schema().index_of(n).unwrap()).clone();
    let ints = col("c_integer");
    let ints = ints.as_primitive::<Int64Type>();
    assert_eq!(ints.value(0), 4242);
    assert_eq!(ints.value(1), i64::MIN);
    assert!(ints.is_null(2));
    let d = col("c_double");
    assert_eq!(d.as_primitive::<Float64Type>().value(1), -1e300);
    let text = col("c_text");
    assert_eq!(
        text.as_string::<i32>().value(0),
        "text value with unicode: café, 日本, 🙂"
    );
    let blob = col("c_blob");
    assert_eq!(blob.as_binary::<i32>().value(0), &[0xde, 0xad, 0xbe, 0xef]);
    let b = col("c_boolean");
    assert!(b.as_boolean().value(0) && !b.as_boolean().value(1));
    let num = col("c_numeric");
    assert_eq!(num.as_string::<i32>().value(0), "12345.678901");
    let untyped = col("c_untyped");
    assert_eq!(untyped.as_string::<i32>().value(1), "42");
    assert!(
        batch
            .schema()
            .field_with_name("c_untyped")
            .unwrap()
            .metadata()
            .contains_key(TRUNCATED_VALUES_METADATA_KEY)
    );
    assert!(
        o.items
            .iter()
            .any(|i| matches!(i, ResultItem::Notice(n) if n.message.contains("c_untyped")))
    );
    // Expressions without a declared type take the first value's class.
    let o = run(
        s.as_mut(),
        "SELECT 1 AS i, 2.5 AS r, 'x' AS t, x'00' AS b, NULL AS n",
    )
    .await
    .unwrap();
    let batch = o
        .items
        .iter()
        .find_map(|i| match i {
            ResultItem::Rows(b) => Some(b.clone()),
            _ => None,
        })
        .unwrap();
    let types: Vec<_> = batch
        .schema()
        .fields()
        .iter()
        .map(|f| f.data_type().clone())
        .collect();
    assert_eq!(
        types,
        vec![
            DataType::Int64,
            DataType::Float64,
            DataType::Utf8,
            DataType::Binary,
            DataType::Utf8
        ]
    );
    // Zero rows still give a header.
    let o = run(s.as_mut(), "SELECT * FROM customers WHERE id < 0")
        .await
        .unwrap();
    assert!(matches!(o.items.first(), Some(ResultItem::ResultSet(_))));
    assert_eq!(o.rows, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn writes_transactions_edits_and_parameters() {
    let dir = tempfile::tempdir().unwrap();
    let db = nine_copy(dir.path());
    let mut s = connect(params(&db)).await;
    assert!(!s.is_read_only());
    let o = run(
        s.as_mut(),
        "UPDATE orders SET status = 'paid' WHERE id <= 10",
    )
    .await
    .unwrap();
    assert_eq!(update_count(&o), Some(Some(10)));
    let o = run(s.as_mut(), "DELETE FROM orders WHERE id < 0")
        .await
        .unwrap();
    assert_eq!(update_count(&o), Some(Some(0)));
    let o = run(
        s.as_mut(),
        "CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT, at TEXT, data BLOB)",
    )
    .await
    .unwrap();
    assert_eq!(update_count(&o), Some(None));
    // Parameters with hostile text stay values.
    let hostile = "x'); DROP TABLE notes; --";
    let o = run_opts(
        s.as_mut(),
        Statement::sql("INSERT INTO notes (body, at, data) VALUES (?, ?, ?) RETURNING id")
            .with_params(vec![
                Param::Text(hostile.into()),
                Param::Timestamp {
                    micros: 1_763_100_000_000_000,
                    utc: true,
                },
                Param::Bytes(vec![0, 1, 2]),
            ]),
        ExecOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(first_i64(&o), Some(1));
    assert_eq!(update_count(&o), Some(Some(1)));
    let o = run_opts(
        s.as_mut(),
        Statement::sql("SELECT body, at FROM notes WHERE id = ?").with_params(vec![Param::Int(1)]),
        ExecOptions::default(),
    )
    .await
    .unwrap();
    let batch = o
        .items
        .iter()
        .find_map(|i| match i {
            ResultItem::Rows(b) => Some(b.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(batch.column(0).as_string::<i32>().value(0), hostile);
    assert_eq!(
        batch.column(1).as_string::<i32>().value(0),
        "2025-11-14 06:00:00.000000Z"
    );
    assert!(matches!(
        run_opts(
            s.as_mut(),
            Statement::sql("SELECT ?").with_params(vec![]),
            ExecOptions::default()
        )
        .await,
        Err(DriverError::InvalidParams(_))
    ));
    assert!(matches!(
        run_opts(
            s.as_mut(),
            Statement::sql("SELECT ?").with_params(vec![Param::Float(f64::NAN)]),
            ExecOptions::default()
        )
        .await,
        Err(DriverError::Unsupported(_))
    ));
    // Grid edits through the dialect.
    let dialect = s.dialect();
    let edit = RowEdit {
        table: vec!["main".into(), "notes".into()],
        change: RowChange::Update {
            key: vec![("id".into(), Param::Int(1))],
            values: vec![("body".into(), Param::Text("edited".into()))],
        },
    };
    let st = dialect.render_edit(&edit).unwrap();
    let o = run_opts(s.as_mut(), st, ExecOptions::default())
        .await
        .unwrap();
    assert_eq!(update_count(&o), Some(Some(1)));
    // Manual commit: rollback undoes, commit keeps.
    s.set_auto_commit(false).await.unwrap();
    assert!(!s.auto_commit());
    run(s.as_mut(), "DELETE FROM notes").await.unwrap();
    assert!(s.in_transaction());
    s.rollback().await.unwrap();
    assert!(!s.in_transaction());
    let o = run(s.as_mut(), "SELECT count(*) FROM notes").await.unwrap();
    assert_eq!(first_i64(&o), Some(1));
    s.savepoint("sp \"1\"").await.unwrap();
    run(s.as_mut(), "INSERT INTO notes (body) VALUES ('b')")
        .await
        .unwrap();
    s.rollback_to_savepoint("sp \"1\"").await.unwrap();
    run(s.as_mut(), "INSERT INTO notes (body) VALUES ('c')")
        .await
        .unwrap();
    s.set_auto_commit(true).await.unwrap();
    assert!(!s.in_transaction());
    let o = run(s.as_mut(), "SELECT count(*) FROM notes").await.unwrap();
    assert_eq!(first_i64(&o), Some(2));
    s.begin().await.unwrap();
    run(s.as_mut(), "DELETE FROM notes").await.unwrap();
    s.commit().await.unwrap();
    assert!(
        s.set_isolation_level(ninesql_driver_api::IsolationLevel::Serializable)
            .await
            .is_ok()
    );
    assert!(
        s.set_isolation_level(ninesql_driver_api::IsolationLevel::ReadCommitted)
            .await
            .is_err()
    );
    // Explain.
    let explain = dialect
        .explain(
            "SELECT * FROM orders WHERE customer_id = 3",
            ninesql_driver_api::ExplainFormat::Text,
        )
        .unwrap();
    let o = run(s.as_mut(), &explain).await.unwrap();
    assert!(o.rows > 0);
    // load_extension is never available, even read-write.
    assert!(
        run(s.as_mut(), "SELECT load_extension('/nonexistent/evil.so')")
            .await
            .is_err()
    );
    // Close rolls back an open transaction.
    s.begin().await.unwrap();
    run(s.as_mut(), "DELETE FROM nine_meta").await.unwrap();
    s.close().await.unwrap();
    let c = rusqlite::Connection::open(&db).unwrap();
    let n: i64 = c
        .query_row("SELECT count(*) FROM nine_meta", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 2);
    // Foreign keys are enforced (bundled default).
    let mut s = connect(params(&db)).await;
    assert!(
        run(s.as_mut(), "DELETE FROM customers WHERE id = 1")
            .await
            .is_err()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metadata_lists_and_generates_ddl() {
    let mut s = connect(ro_params(nine_db())).await;
    let m = s.metadata();
    let root = ObjectPath::root();
    let schemas = m
        .list(&root, ObjectKind::Schema, PageRequest::first())
        .await
        .unwrap();
    let names: Vec<_> = schemas.items.iter().map(|i| i.name().to_owned()).collect();
    assert_eq!(names, vec!["main".to_owned()]);
    let main = root.child(ObjectKind::Schema, "main");
    let tables = m
        .list(&main, ObjectKind::Table, PageRequest::first())
        .await
        .unwrap();
    let names: Vec<_> = tables.items.iter().map(|i| i.name().to_owned()).collect();
    assert_eq!(
        names,
        vec![
            "all_types",
            "big_events",
            "customers",
            "nine_meta",
            "orders",
            "products"
        ]
    );
    // Pages and filters.
    let page = m
        .list(&main, ObjectKind::Table, PageRequest::first().with_limit(2))
        .await
        .unwrap();
    assert_eq!(page.items.len(), 2);
    let next = m
        .list(
            &main,
            ObjectKind::Table,
            PageRequest::first()
                .with_limit(2)
                .with_cursor(page.next_cursor.clone()),
        )
        .await
        .unwrap();
    assert_eq!(next.items[0].name(), "customers");
    let filtered = m
        .list(
            &main,
            ObjectKind::Table,
            PageRequest::first().with_filter(Some("_ev".into())),
        )
        .await
        .unwrap();
    assert_eq!(filtered.items.len(), 1);
    assert_eq!(filtered.items[0].name(), "big_events");
    let wildcard = m
        .list(
            &main,
            ObjectKind::Table,
            PageRequest::first().with_filter(Some("%".into())),
        )
        .await
        .unwrap();
    assert!(
        wildcard.items.is_empty(),
        "the filter is a literal substring"
    );
    let views = m
        .list(&main, ObjectKind::View, PageRequest::first())
        .await
        .unwrap();
    assert_eq!(views.items[0].name(), "customer_order_totals");
    let customers = main.child(ObjectKind::Table, "customers");
    let cols = m
        .list(&customers, ObjectKind::Column, PageRequest::first())
        .await
        .unwrap();
    let names: Vec<_> = cols.items.iter().map(|i| i.name().to_owned()).collect();
    assert_eq!(names, vec!["id", "name", "email", "country", "created_at"]);
    match &cols.items[0].details {
        ninesql_driver_api::ObjectDetails::Column(c) => {
            assert!(c.primary_key);
            assert_eq!(c.data_type, "INTEGER");
            assert_eq!(c.ordinal, 1);
        }
        other => panic!("{other:?}"),
    }
    let ddl = m.ddl(&customers).await.unwrap();
    assert!(ddl.starts_with("CREATE TABLE customers"), "{ddl}");
    let ddl = m
        .ddl(&main.child(ObjectKind::View, "customer_order_totals"))
        .await
        .unwrap();
    assert!(ddl.starts_with("CREATE VIEW"), "{ddl}");
    assert!(
        m.ddl(&main.child(ObjectKind::Table, "missing"))
            .await
            .is_err()
    );
    // An unknown schema never reaches the SQL text.
    let evil = root.child(ObjectKind::Schema, "main\".sqlite_schema; --");
    assert!(matches!(
        m.list(&evil, ObjectKind::Table, PageRequest::first()).await,
        Err(DriverError::InvalidParams(_))
    ));
    // Busy while a result streams.
    let mut stream = s
        .execute(
            Statement::sql("SELECT * FROM big_events"),
            ExecOptions::default(),
        )
        .await
        .unwrap();
    let _ = stream.next().await;
    assert!(matches!(
        m.list(&root, ObjectKind::Schema, PageRequest::first())
            .await,
        Err(DriverError::Busy)
    ));
    drop(stream);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hostile_and_corrupt_files_fail_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let d = SqliteDriver::new();
    // Not a database.
    let junk = dir.path().join("junk.db");
    std::fs::write(&junk, vec![0x5au8; 8192]).unwrap();
    let r = d.connect(ro_params(&junk)).await;
    assert!(matches!(r, Err(DriverError::Connect(_))), "{:?}", r.err());
    // Corrupted pages: connect may succeed, queries fail with an error, never a panic.
    let corrupt = dir.path().join("corrupt.db");
    std::fs::copy(nine_db(), &corrupt).unwrap();
    {
        use std::io::{Seek, SeekFrom, Write};
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .open(&corrupt)
            .unwrap();
        for offset in [4096u64 * 3, 4096 * 50, 4096 * 400, 4096 * 2000] {
            f.seek(SeekFrom::Start(offset)).unwrap();
            f.write_all(&[0xff; 1500]).unwrap();
        }
    }
    if let Ok(mut s) = d.connect(ro_params(&corrupt)).await {
        let mut failed = false;
        for sql in [
            "SELECT * FROM big_events",
            "SELECT * FROM orders",
            "SELECT * FROM customers",
            "PRAGMA integrity_check",
        ] {
            match run(s.as_mut(), sql).await {
                Err(DriverError::Query { .. }) => failed = true,
                Err(e) => panic!("{sql}: unexpected {e:?}"),
                Ok(_) => {}
            }
        }
        assert!(failed, "the corruption went unnoticed");
        // The session is still usable.
        run(s.as_mut(), "SELECT 1").await.unwrap();
    }
    // Truncated file.
    let truncated = dir.path().join("truncated.db");
    let bytes = std::fs::read(nine_db()).unwrap();
    std::fs::write(&truncated, &bytes[..bytes.len() / 3]).unwrap();
    if let Ok(mut s) = d.connect(ro_params(&truncated)).await {
        assert!(
            run(s.as_mut(), "SELECT count(*), max(payload) FROM big_events")
                .await
                .is_err()
        );
    }
    // Invalid UTF-8 in values and in a column name.
    let odd = dir.path().join("odd.db");
    {
        let c = rusqlite::Connection::open(&odd).unwrap();
        c.execute_batch(
            "CREATE TABLE t (a TEXT); INSERT INTO t VALUES (CAST(X'FF61FE62' AS TEXT)); \
             CREATE TABLE u (b TEXT);",
        )
        .unwrap();
        c.execute_batch("PRAGMA writable_schema = ON").unwrap();
        // Column name with invalid UTF-8: rewrite the CREATE statement of `u`.
        let sql = [b"CREATE TABLE u (\"b".as_slice(), &[0xff], b"\" TEXT)"].concat();
        c.execute(
            "UPDATE sqlite_schema SET sql = CAST(? AS TEXT) WHERE name = 'u'",
            [sql],
        )
        .unwrap();
        c.execute_batch("PRAGMA writable_schema = OFF").unwrap();
    }
    let mut s = connect(ro_params(&odd)).await;
    let o = run(s.as_mut(), "SELECT a FROM t").await.unwrap();
    let batch = o
        .items
        .iter()
        .find_map(|i| match i {
            ResultItem::Rows(b) => Some(b.clone()),
            _ => None,
        })
        .unwrap();
    assert!(batch.column(0).as_string::<i32>().value(0).contains('a'));
    assert!(
        batch
            .schema()
            .field(0)
            .metadata()
            .contains_key(TRUNCATED_VALUES_METADATA_KEY)
    );
    // The client library cannot pass a name that is not UTF-8 to the authorizer: the read is
    // denied (fails closed) rather than answered; never a crash.
    let r = run(s.as_mut(), "SELECT * FROM u").await;
    assert!(
        matches!(
            r,
            Err(DriverError::Query { .. } | DriverError::ReadOnly(_)) | Ok(_)
        ),
        "{:?}",
        r.err()
    );
    run_when_free(s.as_mut(), "SELECT 1").await.unwrap();
    // A huge value: the caller's byte budget bounds what SQLite may load (`SQLITE_LIMIT_LENGTH`).
    let big = dir.path().join("big.db");
    {
        let c = rusqlite::Connection::open(&big).unwrap();
        c.execute_batch("CREATE TABLE b (v BLOB); INSERT INTO b VALUES (zeroblob(40000000));")
            .unwrap();
    }
    let mut s = connect(ro_params(&big)).await;
    let r = run_opts(
        s.as_mut(),
        Statement::sql("SELECT v FROM b"),
        ExecOptions::default().with_budget(FetchBudget::default().with_max_bytes(1024 * 1024)),
    )
    .await;
    assert!(
        matches!(&r, Err(DriverError::Query { message, .. }) if message.contains("too big")),
        "{:?}",
        r.as_ref().err()
    );
    let o = run(s.as_mut(), "SELECT v, length(v) FROM b").await.unwrap();
    let batch = o
        .items
        .iter()
        .find_map(|i| match i {
            ResultItem::Rows(b) => Some(b.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        batch.column(0).as_binary::<i32>().value(0).len() as u64,
        FetchBudget::DEFAULT_MAX_VALUE_BYTES
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn paths_symlinks_and_creation() {
    let dir = tempfile::tempdir().unwrap();
    let d = SqliteDriver::new();
    let db = nine_copy(dir.path());
    // A symbolic link is resolved: the real file is opened.
    let link = dir.path().join("link.db");
    std::os::unix::fs::symlink(&db, &link).unwrap();
    let mut s = connect(ro_params(&link)).await;
    assert_eq!(
        first_i64(
            &run(s.as_mut(), "SELECT count(*) FROM customers")
                .await
                .unwrap()
        ),
        Some(1000)
    );
    s.close().await.unwrap();
    // A link to a directory or a device is refused.
    let to_dir = dir.path().join("dir.db");
    std::os::unix::fs::symlink(dir.path(), &to_dir).unwrap();
    assert!(matches!(
        d.connect(ro_params(&to_dir)).await,
        Err(DriverError::InvalidParams(_))
    ));
    let to_dev = dir.path().join("dev.db");
    std::os::unix::fs::symlink("/dev/zero", &to_dev).unwrap();
    assert!(matches!(
        d.connect(ro_params(&to_dev)).await,
        Err(DriverError::InvalidParams(_))
    ));
    // Missing files: created only for read-write sessions that ask for it.
    let fresh = dir.path().join("fresh.db");
    let mut p = ro_params(&fresh);
    p.endpoint = Endpoint::File {
        path: fresh.clone(),
        create_if_missing: true,
    };
    assert!(d.connect(p).await.is_err());
    assert!(!fresh.exists());
    assert!(d.connect(params(&fresh)).await.is_err());
    assert!(!fresh.exists());
    let mut p = params(&fresh);
    p.endpoint = Endpoint::File {
        path: fresh.clone(),
        create_if_missing: true,
    };
    let mut s = connect(p).await;
    run(s.as_mut(), "CREATE TABLE t (a)").await.unwrap();
    s.close().await.unwrap();
    assert!(fresh.is_file());
    // `..` components are resolved.
    let dotted = dir.path().join("sub").join("..").join("nine.db");
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    let mut s = connect(ro_params(&dotted)).await;
    run(s.as_mut(), "SELECT 1").await.unwrap();
    // In-memory, read-write only.
    let mut s = connect(ConnectParams::new(EngineId::SQLITE, Endpoint::InMemory)).await;
    run(s.as_mut(), "CREATE TABLE m (a)").await.unwrap();
    run(s.as_mut(), "INSERT INTO m VALUES (1)").await.unwrap();
    // Accepted properties.
    let mut p = ro_params(&db);
    p.properties
        .insert("busy_timeout".into(), PropertyValue::Plain("100".into()));
    p.properties
        .insert("cache_size".into(), PropertyValue::Plain("-4096".into()));
    let mut s = connect(p).await;
    s.ping().await.unwrap();
    let mut p = ro_params(&db);
    p.properties
        .insert("journal_mode".into(), PropertyValue::Plain("WAL".into()));
    assert!(matches!(
        d.connect(p).await,
        Err(DriverError::InvalidParams(_))
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closed_sessions_and_busy_rules() {
    let mut s = connect(ro_params(nine_db())).await;
    let mut stream = s
        .execute(
            Statement::sql("SELECT * FROM big_events"),
            ExecOptions::default(),
        )
        .await
        .unwrap();
    let _ = stream.next().await;
    assert!(matches!(
        s.execute(Statement::sql("SELECT 1"), ExecOptions::default())
            .await,
        Err(DriverError::Busy)
    ));
    // Draining frees the session.
    let mut n = 0u64;
    while let Some(item) = stream.next().await {
        if let ResultItem::Rows(b) = item.unwrap() {
            n += b.num_rows() as u64;
        }
    }
    assert!(n > 900_000);
    drop(stream);
    run(s.as_mut(), "SELECT 1").await.unwrap();
    assert!(matches!(
        run_opts(
            s.as_mut(),
            Statement::new("db.find()", ninesql_driver_api::QueryLanguage::MongoQuery),
            ExecOptions::default()
        )
        .await,
        Err(DriverError::Unsupported(_))
    ));
    assert!(matches!(
        run(s.as_mut(), "SELECT 1\0").await,
        Err(DriverError::InvalidParams(_))
    ));
    let r = run(s.as_mut(), "SELECT * FROM no_such_table").await;
    assert!(matches!(r, Err(DriverError::Query { .. })), "{:?}", r.err());
    s.close().await.unwrap();
}
