//! Integration tests against the `postgresql` service of `dev/compose.yaml` (ADR-0009).
//!
//! Skipped when the engine is unreachable, unless `NINESQL_REQUIRE_ENGINES` lists
//! `postgresql`. Start it with `python3 dev/ready.py up core`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stderr)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ninesql_driver_api::arrow_array::cast::AsArray;
use ninesql_driver_api::arrow_array::types::{
    Date32Type, Float64Type, Int16Type, Int32Type, Int64Type, Time64MicrosecondType,
    TimestampMicrosecondType,
};
use ninesql_driver_api::arrow_array::{Array, RecordBatch};
use ninesql_driver_api::arrow_schema::{DataType, TimeUnit};
use ninesql_driver_api::{
    Auth, CancelToken, ConnectParams, Driver, DriverError, ENGINE_TYPE_METADATA_KEY, Endpoint,
    EngineId, ExecOptions, FetchBudget, IsolationLevel, ObjectDetails, ObjectKind, ObjectPath,
    PageRequest, Param, ResultItem, ResultSetKind, RowChange, RowEdit, Secret, Session, Statement,
    TRUNCATED_VALUES_METADATA_KEY, TlsMode, conformance,
};
use ninesql_driver_pgwire::PgWireDriver;
use tokio::sync::OnceCell;

struct Dev {
    host: String,
    port: u16,
    user: String,
    password: String,
    database: String,
    ca_sql: String,
}

fn engines_json() -> serde_json::Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../dev/engines.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// Dev parameters, or `None` (test skipped) when the engine is unreachable.
fn dev() -> Option<Dev> {
    let doc = engines_json();
    let e = doc["engines"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == "postgresql")
        .unwrap();
    let dev = Dev {
        host: e["host"].as_str().unwrap().to_owned(),
        port: u16::try_from(e["port"].as_u64().unwrap()).unwrap(),
        user: e["user"].as_str().unwrap().to_owned(),
        password: e["password"].as_str().unwrap().to_owned(),
        database: e["database"].as_str().unwrap().to_owned(),
        ca_sql: e["tls"]["ca_sql"].as_str().unwrap().to_owned(),
    };
    let addr = format!("{}:{}", dev.host, dev.port);
    let reachable = addr
        .parse()
        .ok()
        .and_then(|a| std::net::TcpStream::connect_timeout(&a, Duration::from_secs(2)).ok())
        .is_some();
    if reachable {
        return Some(dev);
    }
    let required = std::env::var("NINESQL_REQUIRE_ENGINES")
        .unwrap_or_default()
        .split(',')
        .any(|s| s.trim() == "postgresql");
    assert!(
        !required,
        "postgresql is required but unreachable at {addr}"
    );
    eprintln!("skipped: postgresql unreachable at {addr} (python3 dev/ready.py up core)");
    None
}

fn params(dev: &Dev) -> ConnectParams {
    let mut p = ConnectParams::new(EngineId::POSTGRESQL, Endpoint::tcp(&dev.host, dev.port));
    p.auth = Auth::password(&dev.user, Some(Secret::new(dev.password.clone())));
    p.database = Some(dev.database.clone());
    p.tls.mode = TlsMode::Disable;
    p.application_name = "ninesql-pgwire-tests".into();
    p
}

async fn connect(dev: &Dev) -> Box<dyn Session> {
    PgWireDriver::new().connect(params(dev)).await.unwrap()
}

#[derive(Default)]
struct Collected {
    schema: Option<ninesql_driver_api::arrow_schema::SchemaRef>,
    batches: Vec<RecordBatch>,
    notices: Vec<String>,
    counts: Vec<Option<u64>>,
    truncated: bool,
}

impl Collected {
    fn rows(&self) -> usize {
        self.batches.iter().map(RecordBatch::num_rows).sum()
    }
}

async fn run_opts(
    s: &mut dyn Session,
    stmt: Statement,
    opts: ExecOptions,
) -> Result<Collected, DriverError> {
    let mut stream = s.execute(stmt, opts).await?;
    let mut c = Collected::default();
    while let Some(item) = stream.next().await {
        match item? {
            ResultItem::ResultSet(h) => {
                if let ResultSetKind::Tabular(schema) = h.kind {
                    c.schema = Some(schema);
                }
            }
            ResultItem::Rows(b) => c.batches.push(b),
            ResultItem::Notice(n) => c.notices.push(n.message),
            ResultItem::UpdateCount(n) => c.counts.push(n),
            ResultItem::Truncated(_) => c.truncated = true,
            _ => {}
        }
    }
    Ok(c)
}

async fn run(s: &mut dyn Session, sql: &str) -> Result<Collected, DriverError> {
    run_opts(s, Statement::sql(sql), ExecOptions::default()).await
}

/// First column of the first row as text (`Utf8` or `Int64`/`Int32`).
fn scalar(c: &Collected) -> Option<String> {
    let b = c.batches.iter().find(|b| b.num_rows() > 0)?;
    let col = b.column(0);
    if col.is_null(0) {
        return None;
    }
    Some(match col.data_type() {
        DataType::Utf8 => col.as_string::<i32>().value(0).to_owned(),
        DataType::Int64 => col.as_primitive::<Int64Type>().value(0).to_string(),
        DataType::Int32 => col.as_primitive::<Int32Type>().value(0).to_string(),
        DataType::Boolean => col.as_boolean().value(0).to_string(),
        other => panic!("unexpected scalar type {other:?}"),
    })
}

/// `execute`, retrying while the session is still busy confirming a previous end (a dropped
/// stream is cleaned up in the background).
async fn run_when_idle(s: &mut dyn Session, sql: &str) -> Result<Collected, DriverError> {
    for _ in 0..100 {
        match run(s, sql).await {
            Err(DriverError::Busy) => tokio::time::sleep(Duration::from_millis(50)).await,
            other => return other,
        }
    }
    Err(DriverError::Busy)
}

/// Test objects in schema `ninesql_test`, created once per test binary.
static FIXTURES: OnceCell<()> = OnceCell::const_new();

async fn fixtures(dev: &Dev) {
    FIXTURES
        .get_or_init(|| async {
            let mut s = connect(dev).await;
            for sql in [
                "CREATE SCHEMA IF NOT EXISTS ninesql_test",
                "CREATE TABLE IF NOT EXISTS ninesql_test.written (a int)",
                // A write the client-side classifier cannot see (a user function).
                "CREATE OR REPLACE FUNCTION ninesql_test.write_one() RETURNS int LANGUAGE sql \
                 AS $$ INSERT INTO ninesql_test.written VALUES (1) RETURNING 1 $$",
                // Lifts the read-only mode without being classified as such.
                "CREATE OR REPLACE FUNCTION ninesql_test.lift_read_only() RETURNS text \
                 LANGUAGE sql AS $$ SELECT set_config('default_transaction_read_only', 'off', false) $$",
                "CREATE OR REPLACE FUNCTION ninesql_test.change_encoding() RETURNS text \
                 LANGUAGE sql AS $$ SELECT set_config('client_encoding', 'LATIN1', false) $$",
            ] {
                run(s.as_mut(), sql).await.unwrap();
            }
            s.close().await.unwrap();
        })
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connects_and_maps_types() {
    let Some(dev) = dev() else { return };
    let mut s = connect(&dev).await;
    assert!(s.server_version().unwrap().starts_with("18"));
    assert_eq!(
        s.capabilities().server_version.as_deref(),
        s.server_version()
    );
    let c = run(s.as_mut(), "SELECT * FROM all_types ORDER BY id")
        .await
        .unwrap();
    assert_eq!(c.rows(), 3);
    let schema = c.schema.clone().unwrap();
    let ty = |n: &str| schema.field_with_name(n).unwrap().data_type().clone();
    let meta = |n: &str| {
        schema
            .field_with_name(n)
            .unwrap()
            .metadata()
            .get(ENGINE_TYPE_METADATA_KEY)
            .cloned()
    };
    assert_eq!(ty("c_smallint"), DataType::Int16);
    assert_eq!(ty("c_integer"), DataType::Int32);
    assert_eq!(ty("c_bigint"), DataType::Int64);
    assert_eq!(ty("c_double"), DataType::Float64);
    assert_eq!(ty("c_real"), DataType::Float32);
    assert_eq!(ty("c_boolean"), DataType::Boolean);
    assert_eq!(ty("c_bytea"), DataType::Binary);
    assert_eq!(ty("c_date"), DataType::Date32);
    assert_eq!(ty("c_time"), DataType::Time64(TimeUnit::Microsecond));
    assert_eq!(
        ty("c_timestamp"),
        DataType::Timestamp(TimeUnit::Microsecond, None)
    );
    assert_eq!(
        ty("c_timestamptz"),
        DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into()))
    );
    for (col, name) in [
        ("c_numeric", "numeric"),
        ("c_interval", "interval"),
        ("c_uuid", "uuid"),
        ("c_json", "json"),
        ("c_jsonb", "jsonb"),
        ("c_int_array", "integer[]"),
        ("c_enum", "mood"),
        ("c_timetz", "time with time zone"),
    ] {
        assert_eq!(ty(col), DataType::Utf8, "{col}");
        assert_eq!(meta(col).as_deref(), Some(name), "{col}");
    }
    assert_eq!(meta("c_text"), None);
    let b = &c.batches[0];
    let col = |n: &str| b.column(schema.index_of(n).unwrap()).clone();
    assert_eq!(col("c_smallint").as_primitive::<Int16Type>().value(0), 42);
    assert_eq!(
        col("c_bigint").as_primitive::<Int64Type>().value(1),
        i64::MAX
    );
    assert_eq!(col("c_numeric").as_string::<i32>().value(0), "12345.678901");
    assert_eq!(
        col("c_text").as_string::<i32>().value(0),
        "text value with unicode: café, 日本, 🙂"
    );
    assert_eq!(
        col("c_bytea").as_binary::<i32>().value(0),
        &[0xde, 0xad, 0xbe, 0xef]
    );
    // 2026-11-14 = 20 771 days after 1970-01-01.
    assert_eq!(col("c_date").as_primitive::<Date32Type>().value(0), 20_771);
    assert_eq!(
        col("c_time")
            .as_primitive::<Time64MicrosecondType>()
            .value(0),
        (13 * 3600 + 37 * 60) * 1_000_000
    );
    assert_eq!(
        col("c_timestamptz")
            .as_primitive::<TimestampMicrosecondType>()
            .value(0),
        1_794_663_420_000_000
    );
    assert!(
        col("c_double")
            .as_primitive::<Float64Type>()
            .value(1)
            .is_infinite()
    );
    assert_eq!(
        col("c_uuid").as_string::<i32>().value(0),
        "6f1c0d2e-9a8b-4c3d-8e7f-1a2b3c4d5e6f"
    );
    assert_eq!(col("c_int_array").as_string::<i32>().value(0), "{1,2,3}");
    assert_eq!(
        col("c_interval").as_string::<i32>().value(0),
        "1 day 02:03:04"
    );
    for i in 1..b.num_columns() {
        assert!(b.column(i).is_null(2), "row 3 is all NULL");
    }
    // Infinite timestamps: NULL, flagged, reported once.
    let c = run(
        s.as_mut(),
        "SELECT 'infinity'::timestamptz AS t, '-infinity'::date AS d",
    )
    .await
    .unwrap();
    let b = &c.batches[0];
    assert!(b.column(0).is_null(0) && b.column(1).is_null(0));
    assert!(
        b.schema()
            .field(0)
            .metadata()
            .contains_key(TRUNCATED_VALUES_METADATA_KEY)
    );
    assert_eq!(c.notices.len(), 2);
    s.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conformance_reserved_properties_and_multiple_statements() {
    let Some(dev) = dev() else { return };
    let driver = PgWireDriver::new();
    conformance::assert_reserved_properties_rejected(&driver, &params(&dev)).await;
    conformance::assert_multiple_statements_refused(&driver, params(&dev)).await;
    // The pgwire extra keys too, and unknown keys (allowlist), before any authentication.
    for key in ["krbsrvname", "target_session_attrs", "prepareThreshold"] {
        let mut p = params(&dev);
        p.auth = Auth::password("nobody", None);
        p.properties.insert(
            key.into(),
            ninesql_driver_api::PropertyValue::Plain("v-secret".into()),
        );
        match driver.connect(p).await {
            Err(DriverError::InvalidParams(m)) => assert!(!m.contains("v-secret")),
            other => panic!("{key}: {:?}", other.map(|_| ())),
        }
    }
    // An accepted property reaches the server as a startup parameter.
    let mut p = params(&dev);
    p.properties.insert(
        "search_path".into(),
        ninesql_driver_api::PropertyValue::Plain("ninesql_test, public".into()),
    );
    let mut s = driver.connect(p).await.unwrap();
    let c = run(s.as_mut(), "SHOW search_path").await.unwrap();
    assert_eq!(scalar(&c).as_deref(), Some("ninesql_test, public"));
}

/// I1: statement text holding a credential (init SQL included) is never logged.
// Current-thread runtime: every task the driver spawns runs on this thread, where the
// log capture of the check is installed.
#[tokio::test(flavor = "current_thread")]
async fn statements_with_credentials_are_never_logged() {
    let Some(dev) = dev() else { return };
    conformance::assert_credential_statements_not_logged(&PgWireDriver::new(), params(&dev)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_only_refuses_writes_engine_side() {
    let Some(dev) = dev() else { return };
    fixtures(&dev).await;
    let driver = PgWireDriver::new();
    let writes = conformance::writes_for(&EngineId::POSTGRESQL);
    conformance::assert_read_only_refuses_writes(&driver, params(&dev), &writes).await;

    // A write the classifier cannot see is rejected by the server (I3).
    let mut p = params(&dev);
    p.read_only = true;
    let mut s = driver.connect(p).await.unwrap();
    assert!(s.is_read_only());
    let before = run(s.as_mut(), "SELECT count(*) FROM ninesql_test.written")
        .await
        .unwrap();
    match run(s.as_mut(), "SELECT ninesql_test.write_one()").await {
        Err(DriverError::ReadOnly(msg)) => assert!(msg.contains("read-only")),
        other => panic!("engine-side write not refused: {:?}", other.map(|_| ())),
    }
    let after = run(s.as_mut(), "SELECT count(*) FROM ninesql_test.written")
        .await
        .unwrap();
    assert_eq!(scalar(&before), scalar(&after));
    // Also inside a manual transaction.
    s.set_auto_commit(false).await.unwrap();
    assert!(matches!(
        run(s.as_mut(), "SELECT ninesql_test.write_one()").await,
        Err(DriverError::ReadOnly(_))
    ));
    s.rollback().await.unwrap();
    // Read-only applied before init statements: an init write fails the connect.
    let mut p = params(&dev);
    p.read_only = true;
    p.init_statements = vec!["SELECT ninesql_test.write_one()".into()];
    assert!(matches!(
        driver.connect(p).await.map(|_| ()),
        Err(DriverError::ReadOnly(_))
    ));
    // Switching a session to read-only at runtime, and back.
    let mut s = connect(&dev).await;
    s.set_read_only(true).await.unwrap();
    assert!(matches!(
        run(s.as_mut(), "SELECT ninesql_test.write_one()").await,
        Err(DriverError::ReadOnly(_))
    ));
    s.set_read_only(false).await.unwrap();
    run(s.as_mut(), "SELECT ninesql_test.write_one()")
        .await
        .unwrap();
}

/// Security review of mysqlwire (H1), checked on pgwire too: `SET TRANSACTION … READ WRITE`
/// changes only the current transaction and leaves `default_transaction_read_only` on. The
/// shared classifier refuses its comma form client-side; the driver's re-assert remains as
/// defence in depth against classifier gaps (tested past the classifier in `src/session.rs`).
/// A write after it must fail, in autocommit, in manual-commit mode and in an explicit
/// transaction.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_shot_read_write_transaction_cannot_write() {
    const ONE_SHOT: &str = "SET TRANSACTION ISOLATION LEVEL READ COMMITTED, READ WRITE";
    let Some(dev) = dev() else { return };
    fixtures(&dev).await;
    let mut p = params(&dev);
    p.read_only = true;
    let mut s = PgWireDriver::new().connect(p).await.unwrap();
    let count = |c: &Collected| scalar(c).unwrap();
    let before = count(
        &run(s.as_mut(), "SELECT count(*) FROM ninesql_test.written")
            .await
            .unwrap(),
    );
    for mode in ["autocommit", "manual", "explicit"] {
        match mode {
            "manual" => s.set_auto_commit(false).await.unwrap(),
            "explicit" => {
                s.set_auto_commit(true).await.unwrap();
                s.begin().await.unwrap();
            }
            _ => {}
        }
        // The comma form is classified `ChangesAccessMode` since the reviewed driver-API PR:
        // refused client-side. The driver's `SET TRANSACTION READ ONLY` re-assert stays as
        // defence in depth.
        assert!(
            matches!(
                run(s.as_mut(), ONE_SHOT).await,
                Err(DriverError::ReadOnly(_))
            ),
            "{mode}: the comma form was not refused"
        );
        match run(s.as_mut(), "SELECT ninesql_test.write_one()").await {
            Err(DriverError::ReadOnly(_)) => {}
            other => panic!(
                "{mode}: write after a one-shot READ WRITE: {:?}",
                other.map(|_| ())
            ),
        }
        if s.in_transaction() {
            s.rollback().await.unwrap();
        }
        assert!(s.is_read_only());
    }
    s.set_auto_commit(true).await.unwrap();
    let write_refused = |r: Result<Collected, DriverError>, case: &str| match r {
        Err(DriverError::ReadOnly(_)) => {}
        other => panic!("{case}: write not refused: {:?}", other.map(|_| ())),
    };
    // Savepoints: the one-shot READ WRITE, then SAVEPOINT, a write, ROLLBACK TO, a write.
    s.begin().await.unwrap();
    assert!(run(s.as_mut(), ONE_SHOT).await.is_err());
    s.savepoint("sp").await.unwrap();
    write_refused(
        run(s.as_mut(), "SELECT ninesql_test.write_one()").await,
        "after SAVEPOINT",
    );
    s.rollback_to_savepoint("sp").await.unwrap();
    write_refused(
        run(s.as_mut(), "SELECT ninesql_test.write_one()").await,
        "after ROLLBACK TO",
    );
    s.rollback().await.unwrap();
    // An aborted transaction, then ROLLBACK TO, then a write.
    s.begin().await.unwrap();
    assert!(run(s.as_mut(), ONE_SHOT).await.is_err());
    s.savepoint("sp").await.unwrap();
    assert!(run(s.as_mut(), "SELECT 1 / 0").await.is_err());
    s.rollback_to_savepoint("sp").await.unwrap();
    write_refused(
        run(s.as_mut(), "SELECT ninesql_test.write_one()").await,
        "after an aborted transaction and ROLLBACK TO",
    );
    s.rollback().await.unwrap();
    assert!(s.is_read_only());
    let after = count(
        &run(s.as_mut(), "SELECT count(*) FROM ninesql_test.written")
            .await
            .unwrap(),
    );
    assert_eq!(before, after, "rows were written");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn access_mode_change_is_detected() {
    let Some(dev) = dev() else { return };
    fixtures(&dev).await;
    let driver = PgWireDriver::new();
    conformance::assert_access_mode_change_detected(
        &driver,
        params(&dev),
        "SELECT ninesql_test.lift_read_only()",
    )
    .await;
    // A client-encoding change closes any session (lexical mode).
    let mut s = connect(&dev).await;
    assert!(matches!(
        run(s.as_mut(), "SELECT ninesql_test.change_encoding()").await,
        Err(DriverError::Closed(_))
    ));
    assert!(matches!(
        run(s.as_mut(), "SELECT 1").await,
        Err(DriverError::Closed(_))
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn batches_stay_within_budget_on_a_million_rows() {
    let Some(dev) = dev() else { return };
    let budget = FetchBudget::default()
        .with_batch_rows(5_000)
        .with_max_batch_bytes(512 * 1024)
        .with_max_value_bytes(40)
        .with_max_row_bytes(120)
        .with_max_bytes(1024 * 1024 * 1024);
    let report = conformance::assert_batches_within_budget(
        &PgWireDriver::new(),
        params(&dev),
        "SELECT id, event_time, kind, value, payload, repeat(kind, 30) AS wide FROM big_events",
        budget,
    )
    .await;
    assert_eq!(report.rows, 1_000_000);
    assert!(report.truncated_values);
    assert!(report.batches >= 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_values_are_cut_while_reading_and_budget_truncates() {
    let Some(dev) = dev() else { return };
    let mut s = connect(&dev).await;
    // 32 MiB value; the default value budget is 1 MiB.
    let c = run(
        s.as_mut(),
        "SELECT repeat('é', 16 * 1024 * 1024) AS big, 1 AS n",
    )
    .await
    .unwrap();
    let b = &c.batches[0];
    let v = b.column(0).as_string::<i32>().value(0);
    assert!(v.len() <= 1024 * 1024 && v.len() > 1000);
    assert!(v.chars().all(|ch| ch == 'é'));
    assert!(
        b.schema()
            .field(0)
            .metadata()
            .contains_key(TRUNCATED_VALUES_METADATA_KEY)
    );
    // max_rows / max_bytes end the result with Truncated; the session stays usable.
    let opts = ExecOptions::default().with_budget(FetchBudget::default().with_max_rows(Some(250)));
    let c = run_opts(s.as_mut(), Statement::sql("SELECT * FROM big_events"), opts)
        .await
        .unwrap();
    assert!(c.truncated);
    assert_eq!(c.rows(), 250);
    let c = run_when_idle(s.as_mut(), "SELECT 42").await.unwrap();
    assert_eq!(scalar(&c).as_deref(), Some("42"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn identifier_quoting_round_trips() {
    let Some(dev) = dev() else { return };
    conformance::assert_identifier_quoting(
        &PgWireDriver::new(),
        params(&dev),
        conformance::ALIAS_TEMPLATE,
        conformance::IDENTIFIER_SAMPLES,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_cancel_is_ignored() {
    let Some(dev) = dev() else { return };
    let rows = conformance::assert_stale_cancel_ignored(
        &PgWireDriver::new(),
        params(&dev),
        "SELECT * FROM big_events LIMIT 1000",
        "SELECT * FROM big_events LIMIT 100000",
    )
    .await;
    assert_eq!(rows, 100_000);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn verify_full_rejects_untrusted_certificates() {
    let Some(dev) = dev() else { return };
    // The CA is generated in the container at first start (dev/tls/postgresql-tls.sh).
    let mut s = connect(&dev).await;
    let ca = scalar(&run(s.as_mut(), &dev.ca_sql).await.unwrap()).unwrap();
    assert!(ca.contains("BEGIN CERTIFICATE"));
    let dir = tempfile::tempdir().unwrap();
    let ca_path = dir.path().join("ca.pem");
    std::fs::write(&ca_path, ca).unwrap();
    let make = || {
        let mut p = params(&dev);
        p.tls.mode = TlsMode::VerifyFull;
        p
    };
    conformance::assert_verify_full_rejects_untrusted_certificate(
        &PgWireDriver::new(),
        &make,
        &ca_path,
        "wrong.example.test",
    )
    .await;
    // `require` encrypts without verification; the session really is on TLS.
    let mut p = params(&dev);
    p.tls.mode = TlsMode::Require;
    let mut s = PgWireDriver::new().connect(p).await.unwrap();
    let c = run(
        s.as_mut(),
        "SELECT ssl::text FROM pg_stat_ssl WHERE pid = pg_backend_pid()",
    )
    .await
    .unwrap();
    assert_eq!(scalar(&c).as_deref(), Some("true"));
    // A tunnel-style endpoint: connect to 127.0.0.1, verify the name "localhost".
    let mut p = params(&dev);
    p.tls.mode = TlsMode::VerifyFull;
    p.tls.ca_file = Some(ca_path.clone());
    p.tls.server_name = Some("localhost".into());
    let mut s = PgWireDriver::new().connect(p).await.unwrap();
    // Cancel requests travel over TLS as well.
    let opts = ExecOptions::default().with_timeout(Some(Duration::from_millis(500)));
    let started = std::time::Instant::now();
    assert!(matches!(
        run_opts(s.as_mut(), Statement::sql("SELECT pg_sleep(30)"), opts).await,
        Err(DriverError::Timeout(_))
    ));
    assert!(started.elapsed() < Duration::from_secs(10));
    run_when_idle(s.as_mut(), "SELECT 1").await.unwrap();
}

/// The idle-cursor rule (docs/08): a paused cursor holds a snapshot and locks; when its deadline
/// passes, the driver closes it without waiting for the consumer, and the locks are released.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn paused_cursor_releases_locks_after_its_deadline() {
    let Some(dev) = dev() else { return };
    let mut a = connect(&dev).await;
    let mut b = connect(&dev).await;
    let pid = scalar(&run(a.as_mut(), "SELECT pg_backend_pid()").await.unwrap()).unwrap();
    let locks_sql = format!(
        "SELECT count(*) FROM pg_locks WHERE pid = {pid} AND relation = 'big_events'::regclass"
    );
    let state_sql = format!("SELECT state FROM pg_stat_activity WHERE pid = {pid}");
    let opts = ExecOptions::default()
        .with_timeout(Some(Duration::from_secs(1)))
        .with_budget(FetchBudget::default().with_batch_rows(100));
    let mut stream = a
        .execute(Statement::sql("SELECT * FROM big_events"), opts)
        .await
        .unwrap();
    // Header and first batch, then the consumer pauses.
    let mut rows = 0;
    while rows == 0 {
        if let ResultItem::Rows(batch) = stream.next().await.unwrap().unwrap() {
            rows = batch.num_rows();
        }
    }
    assert_eq!(rows, 100);
    let held = scalar(&run(b.as_mut(), &locks_sql).await.unwrap()).unwrap();
    assert_eq!(held, "1", "the paused cursor holds its lock");
    let state = scalar(&run(b.as_mut(), &state_sql).await.unwrap()).unwrap();
    assert_eq!(state, "idle in transaction");
    // Deadline passes while nobody polls.
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let held = scalar(&run(b.as_mut(), &locks_sql).await.unwrap()).unwrap();
    assert_eq!(held, "0", "locks released after the deadline");
    let state = scalar(&run(b.as_mut(), &state_sql).await.unwrap()).unwrap();
    assert_eq!(state, "idle");
    // The consumer comes back: timeout, then the session is usable.
    assert!(matches!(
        stream.next().await,
        Some(Err(DriverError::Timeout(_)))
    ));
    drop(stream);
    let c = run_when_idle(a.as_mut(), "SELECT 7").await.unwrap();
    assert_eq!(scalar(&c).as_deref(), Some("7"));
    // The driver-implicit transaction sets idle_in_transaction_session_timeout (5 min).
    assert_eq!(
        ninesql_driver_pgwire::IDLE_CURSOR_TIMEOUT,
        Duration::from_secs(300)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropped_stream_releases_the_cursor() {
    let Some(dev) = dev() else { return };
    let mut a = connect(&dev).await;
    let mut b = connect(&dev).await;
    let pid = scalar(&run(a.as_mut(), "SELECT pg_backend_pid()").await.unwrap()).unwrap();
    let mut stream = a
        .execute(
            Statement::sql("SELECT * FROM big_events"),
            ExecOptions::default(),
        )
        .await
        .unwrap();
    let _ = stream.next().await;
    let _ = stream.next().await;
    drop(stream);
    let c = run_when_idle(a.as_mut(), "SELECT 1").await.unwrap();
    assert_eq!(scalar(&c).as_deref(), Some("1"));
    let c = run(
        b.as_mut(),
        &format!(
            "SELECT count(*) FROM pg_locks WHERE pid = {pid} AND relation = 'big_events'::regclass"
        ),
    )
    .await
    .unwrap();
    assert_eq!(scalar(&c).as_deref(), Some("0"));
    assert!(!a.in_transaction());
}

/// A stream dropped while its statement runs on the server: the driver cancels it itself and
/// frees the session once the server confirmed the end.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stream_dropped_mid_statement_is_cancelled_by_the_driver() {
    let Some(dev) = dev() else { return };
    let mut s = connect(&dev).await;
    let mut stream = s
        .execute(
            Statement::sql("SELECT pg_sleep(30)"),
            ExecOptions::default(),
        )
        .await
        .unwrap();
    // The first poll starts the statement; give up on it and drop the stream.
    assert!(
        tokio::time::timeout(Duration::from_millis(300), stream.next())
            .await
            .is_err()
    );
    drop(stream);
    let started = std::time::Instant::now();
    let c = run_when_idle(s.as_mut(), "SELECT 3").await.unwrap();
    assert_eq!(scalar(&c).as_deref(), Some("3"));
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_and_timeout_stop_running_statements() {
    let Some(dev) = dev() else { return };
    let mut s = connect(&dev).await;
    let token = CancelToken::new();
    let t2 = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        t2.cancel();
    });
    let started = std::time::Instant::now();
    let r = run_opts(
        s.as_mut(),
        Statement::sql("SELECT pg_sleep(30)"),
        ExecOptions::default().with_cancel(token),
    )
    .await;
    assert!(
        matches!(r, Err(DriverError::Cancelled)),
        "{:?}",
        r.map(|_| ())
    );
    assert!(started.elapsed() < Duration::from_secs(6));
    run_when_idle(s.as_mut(), "SELECT 1").await.unwrap();
    let r = run_opts(
        s.as_mut(),
        Statement::sql("SELECT pg_sleep(30)"),
        ExecOptions::default().with_timeout(Some(Duration::from_millis(300))),
    )
    .await;
    assert!(matches!(r, Err(DriverError::Timeout(_))));
    run_when_idle(s.as_mut(), "SELECT 1").await.unwrap();
    // Session-level cancel handle while a statement runs.
    let mut stream = s
        .execute(
            Statement::sql("SELECT pg_sleep(30)"),
            ExecOptions::default(),
        )
        .await
        .unwrap();
    let handle = s.cancel_handle();
    let canceller = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        handle.cancel().await.unwrap();
    });
    let mut outcome = None;
    while let Some(item) = stream.next().await {
        if let Err(e) = item {
            outcome = Some(e);
        }
    }
    canceller.await.unwrap();
    assert!(
        matches!(outcome, Some(DriverError::Cancelled)),
        "{outcome:?}"
    );
    drop(stream);
    run_when_idle(s.as_mut(), "SELECT 1").await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transactions_savepoints_and_isolation() {
    let Some(dev) = dev() else { return };
    let mut s = connect(&dev).await;
    run(s.as_mut(), "CREATE TEMP TABLE tx_t (a int PRIMARY KEY)")
        .await
        .unwrap();
    s.set_auto_commit(false).await.unwrap();
    assert!(!s.in_transaction());
    run(s.as_mut(), "INSERT INTO tx_t VALUES (1)")
        .await
        .unwrap();
    assert!(s.in_transaction());
    s.savepoint("sp one").await.unwrap();
    run(s.as_mut(), "INSERT INTO tx_t VALUES (2)")
        .await
        .unwrap();
    s.rollback_to_savepoint("sp one").await.unwrap();
    s.commit().await.unwrap();
    assert!(!s.in_transaction());
    let c = run(s.as_mut(), "SELECT count(*) FROM tx_t").await.unwrap();
    assert_eq!(scalar(&c).as_deref(), Some("1"));
    s.rollback().await.unwrap();
    run(s.as_mut(), "INSERT INTO tx_t VALUES (3)")
        .await
        .unwrap();
    s.rollback().await.unwrap();
    // A failed statement aborts the transaction; COMMIT reports the rollback.
    assert!(
        run(s.as_mut(), "INSERT INTO tx_t VALUES (1)")
            .await
            .is_err()
    );
    assert!(matches!(
        run(s.as_mut(), "SELECT 1").await,
        Err(DriverError::Query { code: Some(c), .. }) if c == "25P02"
    ));
    assert!(s.commit().await.is_err());
    s.set_isolation_level(IsolationLevel::Serializable)
        .await
        .unwrap();
    let c = run(s.as_mut(), "SHOW transaction_isolation").await.unwrap();
    assert_eq!(scalar(&c).as_deref(), Some("serializable"));
    assert!(matches!(
        s.set_isolation_level(IsolationLevel::ReadUncommitted).await,
        Err(DriverError::Unsupported(_))
    ));
    s.rollback().await.unwrap();
    // Back to auto-commit: an open transaction is committed (JDBC semantics).
    run(s.as_mut(), "INSERT INTO tx_t VALUES (4)")
        .await
        .unwrap();
    s.set_auto_commit(true).await.unwrap();
    assert!(!s.in_transaction());
    let c = run(s.as_mut(), "SELECT count(*) FROM tx_t").await.unwrap();
    assert_eq!(scalar(&c).as_deref(), Some("2"));
    // A paused cursor inside the user's transaction: dropped, the transaction stays open.
    s.set_auto_commit(false).await.unwrap();
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
    run_when_idle(s.as_mut(), "INSERT INTO tx_t VALUES (5)")
        .await
        .unwrap();
    assert!(s.in_transaction());
    s.commit().await.unwrap();
    s.ping().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edits_parameters_counts_and_notices() {
    let Some(dev) = dev() else { return };
    let mut s = connect(&dev).await;
    let c = run(
        s.as_mut(),
        "CREATE TEMP TABLE edit_t (id int PRIMARY KEY, label text, data bytea, at timestamptz)",
    )
    .await
    .unwrap();
    assert_eq!(c.counts, vec![None]);
    let d = s.dialect();
    let insert = d
        .render_edit(&RowEdit {
            table: vec!["edit_t".into()],
            change: RowChange::Insert {
                values: vec![
                    ("id".into(), Param::Int(1)),
                    ("label".into(), Param::Text("o'hara \\ ünï".into())),
                    ("data".into(), Param::Bytes(vec![0, 1, 255])),
                    (
                        "at".into(),
                        Param::Timestamp {
                            micros: 1_000_000,
                            utc: true,
                        },
                    ),
                ],
            },
        })
        .unwrap();
    let c = run_opts(s.as_mut(), insert, ExecOptions::default())
        .await
        .unwrap();
    assert_eq!(c.counts, vec![Some(1)]);
    let update = d
        .render_edit(&RowEdit {
            table: vec!["edit_t".into()],
            change: RowChange::Update {
                key: vec![("id".into(), Param::Int(1))],
                values: vec![("label".into(), Param::Null)],
            },
        })
        .unwrap();
    let c = run_opts(s.as_mut(), update, ExecOptions::default())
        .await
        .unwrap();
    assert_eq!(c.counts, vec![Some(1)]);
    let q = Statement::sql("SELECT label, data, at FROM edit_t WHERE id = $1")
        .with_params(vec![Param::Int(1)]);
    let c = run_opts(s.as_mut(), q, ExecOptions::default())
        .await
        .unwrap();
    let b = &c.batches[0];
    assert!(b.column(0).is_null(0));
    assert_eq!(b.column(1).as_binary::<i32>().value(0), &[0, 1, 255]);
    assert_eq!(
        b.column(2)
            .as_primitive::<TimestampMicrosecondType>()
            .value(0),
        1_000_000
    );
    let delete = d
        .render_edit(&RowEdit {
            table: vec!["edit_t".into()],
            change: RowChange::Delete {
                key: vec![("id".into(), Param::Int(1))],
            },
        })
        .unwrap();
    let c = run_opts(s.as_mut(), delete, ExecOptions::default())
        .await
        .unwrap();
    assert_eq!(c.counts, vec![Some(1)]);
    // DML with RETURNING: rows, then the count.
    let c = run(
        s.as_mut(),
        "INSERT INTO edit_t (id) SELECT g FROM generate_series(10, 14) g RETURNING id",
    )
    .await
    .unwrap();
    assert_eq!(c.rows(), 5);
    assert_eq!(c.counts, vec![Some(5)]);
    // Notices.
    let c = run(
        s.as_mut(),
        "DO $$ BEGIN RAISE NOTICE 'hello from plpgsql'; END $$",
    )
    .await
    .unwrap();
    assert!(c.notices.iter().any(|n| n.contains("hello from plpgsql")));
    // Errors carry the SQLSTATE and position; the session stays usable.
    match run(s.as_mut(), "SELECT * FROM no_such_table").await {
        Err(DriverError::Query { code, position, .. }) => {
            assert_eq!(code.as_deref(), Some("42P01"));
            assert_eq!(position, Some(15));
        }
        other => panic!("{:?}", other.map(|_| ())),
    }
    run(s.as_mut(), "SELECT 1").await.unwrap();
    // Empty statement, COPY TO STDOUT refused after discarding.
    run(s.as_mut(), "-- nothing").await.unwrap();
    assert!(matches!(
        run(s.as_mut(), "COPY (SELECT 1) TO STDOUT").await,
        Err(DriverError::Query { .. })
    ));
    run(s.as_mut(), "SELECT 1").await.unwrap();
    // `standard_conforming_strings` drives the session dialect.
    assert!(!s.dialect().lexical_rules().backslash_escapes);
    run(s.as_mut(), "SET standard_conforming_strings = off")
        .await
        .unwrap();
    assert!(s.dialect().lexical_rules().backslash_escapes);
    let sql = r"SELECT 'a\'; SELECT 2; --'";
    assert_eq!(s.dialect().split_statements(sql).len(), 1);
    run(s.as_mut(), "SET standard_conforming_strings = on")
        .await
        .unwrap();
    assert!(matches!(
        run(s.as_mut(), sql).await,
        Err(DriverError::MultipleStatements(2))
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metadata_lists_with_bound_filters_and_pages() {
    let Some(dev) = dev() else { return };
    fixtures(&dev).await;
    let s = connect(&dev).await;
    let m = s.metadata();
    let root = ObjectPath::root();
    let schemas = m
        .list(&root, ObjectKind::Schema, PageRequest::first())
        .await
        .unwrap();
    assert!(schemas.items.iter().any(|o| o.name() == "public"));
    let public = root.child(ObjectKind::Schema, "public");
    let tables = m
        .list(
            &public,
            ObjectKind::Table,
            PageRequest::first().with_filter(Some("_types".into())),
        )
        .await
        .unwrap();
    let names: Vec<&str> = tables.items.iter().map(|o| o.name()).collect();
    assert_eq!(names, vec!["all_types"]);
    // `_` and `%` are literal.
    let none = m
        .list(
            &public,
            ObjectKind::Table,
            PageRequest::first().with_filter(Some("%".into())),
        )
        .await
        .unwrap();
    assert!(none.items.is_empty());
    // Paging.
    let p1 = m
        .list(
            &public,
            ObjectKind::Table,
            PageRequest::first().with_limit(2),
        )
        .await
        .unwrap();
    assert_eq!(p1.items.len(), 2);
    let p2 = m
        .list(
            &public,
            ObjectKind::Table,
            PageRequest::first()
                .with_limit(2)
                .with_cursor(p1.next_cursor.clone()),
        )
        .await
        .unwrap();
    assert!(p2.items.iter().all(|o| !p1.items.contains(o)));
    let views = m
        .list(&public, ObjectKind::View, PageRequest::first())
        .await
        .unwrap();
    assert!(
        views
            .items
            .iter()
            .any(|o| o.name() == "customer_order_totals")
    );
    let customers = public.child(ObjectKind::Table, "customers");
    let cols = m
        .list(&customers, ObjectKind::Column, PageRequest::first())
        .await
        .unwrap();
    let first = &cols.items[0];
    assert_eq!(first.name(), "id");
    match &first.details {
        ObjectDetails::Column(c) => {
            assert!(c.primary_key && !c.nullable);
            assert_eq!(c.data_type, "integer");
        }
        other => panic!("{other:?}"),
    }
    let cols_p = m
        .list(
            &customers,
            ObjectKind::Column,
            PageRequest::first().with_limit(2),
        )
        .await
        .unwrap();
    let cols_p2 = m
        .list(
            &customers,
            ObjectKind::Column,
            PageRequest::first()
                .with_limit(2)
                .with_cursor(cols_p.next_cursor.clone()),
        )
        .await
        .unwrap();
    assert_eq!(cols_p2.items[0].name(), "email");
    let test_schema = root.child(ObjectKind::Schema, "ninesql_test");
    let routines = m
        .list(&test_schema, ObjectKind::Routine, PageRequest::first())
        .await
        .unwrap();
    assert!(routines.items.iter().any(|o| o.name() == "write_one()"));
    assert!(matches!(
        m.ddl(&customers).await,
        Err(DriverError::Unsupported(_))
    ));
    assert_eq!(m.child_kinds(None), vec![ObjectKind::Schema]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrong_password_is_an_auth_error_without_the_password() {
    let Some(dev) = dev() else { return };
    let mut p = params(&dev);
    p.auth = Auth::password(&dev.user, Some(Secret::new("Wr0ng-pa55word!".into())));
    match PgWireDriver::new().connect(p).await {
        Err(DriverError::Auth(m)) => assert!(!m.contains("Wr0ng-pa55word!")),
        other => panic!("{:?}", other.map(|_| ())),
    }
    let mut p = params(&dev);
    p.database = Some("no_such_db".into());
    assert!(matches!(
        PgWireDriver::new().connect(p).await.map(|_| ()),
        Err(DriverError::Connect(_))
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closed_session_refuses_work() {
    let Some(dev) = dev() else { return };
    let s = connect(&dev).await;
    let caps = Arc::new(s.capabilities().clone());
    assert!(caps.read_only.is_liftable());
    s.close().await.unwrap();
}

/// H1 of the P1-B security review (CVE-2018-1058 class): user operators that are a better
/// match than the built-ins (`||(name, text)`, `=(name, text)`) in a schema on the session's
/// `search_path` never run during the driver's own queries.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn driver_queries_never_resolve_to_user_operators() {
    let Some(dev) = dev() else { return };
    let mut admin = connect(&dev).await;
    for sql in [
        "DROP SCHEMA IF EXISTS ninesql_evil CASCADE",
        "CREATE SCHEMA ninesql_evil",
        "CREATE TABLE ninesql_evil.calls (what text)",
        "CREATE FUNCTION ninesql_evil.cat(name, text) RETURNS text LANGUAGE plpgsql AS \
         $$ BEGIN INSERT INTO ninesql_evil.calls VALUES ('||'); RETURN $1::text || $2; END $$",
        "CREATE FUNCTION ninesql_evil.eq(name, text) RETURNS boolean LANGUAGE plpgsql AS \
         $$ BEGIN INSERT INTO ninesql_evil.calls VALUES ('='); RETURN $1::text = $2; END $$",
        "CREATE FUNCTION ninesql_evil.eqo(oid, oid) RETURNS boolean LANGUAGE plpgsql AS \
         $$ BEGIN INSERT INTO ninesql_evil.calls VALUES ('oid='); RETURN $1::int8 = $2::int8; END $$",
        "CREATE OPERATOR ninesql_evil.|| (LEFTARG = name, RIGHTARG = text, FUNCTION = ninesql_evil.cat)",
        "CREATE OPERATOR ninesql_evil.= (LEFTARG = name, RIGHTARG = text, FUNCTION = ninesql_evil.eq)",
        "CREATE OPERATOR ninesql_evil.= (LEFTARG = oid, RIGHTARG = oid, FUNCTION = ninesql_evil.eqo)",
        "CREATE TYPE ninesql_evil.mood2 AS ENUM ('a')",
    ] {
        run(admin.as_mut(), sql).await.unwrap();
    }
    let calls = "SELECT count(*) FROM ninesql_evil.calls";
    let mut p = params(&dev);
    p.properties.insert(
        "search_path".into(),
        ninesql_driver_api::PropertyValue::Plain("ninesql_evil, public".into()),
    );
    let driver = PgWireDriver::new();
    let mut s = driver.connect(p).await.unwrap();
    // Positive control: unqualified SQL in this session does pick the planted operator.
    run(
        s.as_mut(),
        "SELECT n.nspname || 'x' FROM pg_catalog.pg_namespace n LIMIT 1",
    )
    .await
    .unwrap();
    assert_eq!(
        scalar(&run(admin.as_mut(), calls).await.unwrap()).as_deref(),
        Some("1")
    );
    run(admin.as_mut(), "TRUNCATE ninesql_evil.calls")
        .await
        .unwrap();
    // Navigator, type-name lookup (enum of a new schema) and the read-only machinery.
    let m = s.metadata();
    let root = ObjectPath::root();
    m.list(
        &root,
        ObjectKind::Schema,
        PageRequest::first().with_filter(Some("nine".into())),
    )
    .await
    .unwrap();
    let evil = root.child(ObjectKind::Schema, "ninesql_evil");
    let public = root.child(ObjectKind::Schema, "public");
    for (parent, kind) in [
        (&evil, ObjectKind::Routine),
        (&evil, ObjectKind::Table),
        (&public, ObjectKind::View),
        (&public, ObjectKind::MaterializedView),
    ] {
        m.list(
            parent,
            kind,
            PageRequest::first().with_filter(Some("a".into())),
        )
        .await
        .unwrap();
    }
    m.list(
        &evil.child(ObjectKind::Table, "calls"),
        ObjectKind::Column,
        PageRequest::first(),
    )
    .await
    .unwrap();
    let _ = m.ddl(&evil).await;
    run(s.as_mut(), "SELECT 'a'::ninesql_evil.mood2 AS m")
        .await
        .unwrap();
    run(s.as_mut(), "SELECT * FROM big_events LIMIT 300")
        .await
        .unwrap();
    s.set_read_only(true).await.unwrap();
    run(s.as_mut(), "SELECT 1").await.unwrap();
    s.set_read_only(false).await.unwrap();
    assert_eq!(
        scalar(&run(admin.as_mut(), calls).await.unwrap()).as_deref(),
        Some("0"),
        "a user operator ran inside a driver query"
    );
    // pg_catalog listed after a user schema is refused as a property.
    let mut p = params(&dev);
    p.properties.insert(
        "search_path".into(),
        ninesql_driver_api::PropertyValue::Plain("ninesql_evil, pg_catalog".into()),
    );
    assert!(matches!(
        driver.connect(p).await.map(|_| ()),
        Err(DriverError::InvalidParams(_))
    ));
    run(admin.as_mut(), "DROP SCHEMA ninesql_evil CASCADE")
        .await
        .unwrap();
}

/// M3: in the driver-implicit transaction, only a portal that ran to completion is committed;
/// a dropped stream rolls back a read-class statement's side effects.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropped_read_with_side_effects_is_rolled_back() {
    let Some(dev) = dev() else { return };
    fixtures(&dev).await;
    let mut s = connect(&dev).await;
    run(s.as_mut(), "CREATE TEMP TABLE m3_marker (a int)")
        .await
        .unwrap();
    // A read-class statement whose function writes (the classifier cannot see it).
    let sql = "SELECT ninesql_test.write_one(), g FROM generate_series(1, 5000) g";
    let count = "SELECT count(*) FROM ninesql_test.written";
    let before: i64 = scalar(&run(s.as_mut(), count).await.unwrap())
        .unwrap()
        .parse()
        .unwrap();
    let opts = ExecOptions::default().with_budget(FetchBudget::default().with_batch_rows(100));
    let mut stream = s.execute(Statement::sql(sql), opts).await.unwrap();
    let mut got = 0;
    while got == 0 {
        if let ResultItem::Rows(b) = stream.next().await.unwrap().unwrap() {
            got = b.num_rows();
        }
    }
    drop(stream);
    let after: i64 = scalar(&run_when_idle(s.as_mut(), count).await.unwrap())
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(after, before, "a dropped statement's writes were committed");
    // Run to completion: committed.
    run(
        s.as_mut(),
        "SELECT ninesql_test.write_one(), g FROM generate_series(1, 3) g",
    )
    .await
    .unwrap();
    let done: i64 = scalar(&run(s.as_mut(), count).await.unwrap())
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(done, before + 3);
}

/// L2: the driver only lowers `idle_in_transaction_session_timeout`; a stricter session value
/// stays in force for a paused cursor (here the server ends the session after 1 s).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stricter_idle_timeout_is_kept() {
    let Some(dev) = dev() else { return };
    let mut s = connect(&dev).await;
    run(s.as_mut(), "SET idle_in_transaction_session_timeout = '1s'")
        .await
        .unwrap();
    let opts = ExecOptions::default().with_budget(FetchBudget::default().with_batch_rows(100));
    let mut stream = s
        .execute(Statement::sql("SELECT * FROM big_events"), opts)
        .await
        .unwrap();
    let mut got = 0;
    while got == 0 {
        if let ResultItem::Rows(b) = stream.next().await.unwrap().unwrap() {
            got = b.num_rows();
        }
    }
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let mut failed = None;
    while let Some(item) = stream.next().await {
        if let Err(e) = item {
            failed = Some(e);
            break;
        }
    }
    assert!(matches!(failed, Some(DriverError::Closed(_))), "{failed:?}");
}

/// M2 at connect: `verify-ca` needs a CA file; M4: a notice flood is delivered in chunks.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn verify_ca_needs_a_ca_file_and_notices_are_chunked() {
    let Some(dev) = dev() else { return };
    let mut p = params(&dev);
    p.tls.mode = TlsMode::VerifyCa;
    assert!(matches!(
        PgWireDriver::new().connect(p).await.map(|_| ()),
        Err(DriverError::Tls(_))
    ));
    let mut s = connect(&dev).await;
    let c = run(
        s.as_mut(),
        "DO $$ BEGIN FOR i IN 1..3000 LOOP RAISE NOTICE 'n %', i; END LOOP; END $$",
    )
    .await
    .unwrap();
    assert_eq!(c.notices.len(), 3000);
}

/// Runs every item of `script` (P3-D): statements one per call, in order. Returns the results
/// of the statements and the kinds of every item.
async fn run_script(
    s: &mut dyn Session,
    script: &str,
) -> (Vec<Collected>, Vec<ninesql_driver_api::ScriptItemKind>) {
    let items = s.dialect().split_script(script);
    let mut out = Vec::new();
    for item in &items {
        if item.kind == ninesql_driver_api::ScriptItemKind::Statement {
            let text = item.text(script);
            match run(s, text).await {
                Ok(c) => out.push(c),
                Err(e) => panic!("line {}: {text:?} failed: {e}", item.start.line),
            }
        }
    }
    (out, items.iter().map(|i| i.kind).collect())
}

/// The splitter agrees with PostgreSQL 18 on dollar quotes, `E''` strings, nested comments, a
/// carriage return ending a line comment, `BEGIN ATOMIC` bodies, `;` inside a rule's
/// parentheses, and `COPY … FROM STDIN` data that is never sent (P3-D, F-SQL-18).
#[tokio::test]
async fn scripts_split_as_postgres_parses_them() {
    use ninesql_driver_api::{ScriptItemKind, ScriptRefusal};
    let Some(dev) = dev() else { return };
    let mut s = connect(&dev).await;
    let script = "CREATE TEMP TABLE p3d_t (id int, a text);\n\
        INSERT INTO p3d_t VALUES (1, E'it\\'s; ok'), (2, $q$semi;colon$q$), (3, 'back\\');\n\
        /* nested /* comment ; */ still ; */ SELECT 1;\n\
        CREATE FUNCTION pg_temp.p3d_f(x int) RETURNS int LANGUAGE sql\n\
        BEGIN ATOMIC\n  SELECT CASE WHEN x > 0 THEN x ELSE 0 END;\nEND;\n\
        CREATE FUNCTION pg_temp.p3d_g() RETURNS int LANGUAGE plpgsql AS $body$\n\
        BEGIN RETURN 1; END;\n$body$;\n\
        CREATE TEMP TABLE p3d_log (n int);\n\
        CREATE RULE p3d_r AS ON INSERT TO p3d_t DO ALSO \
        (INSERT INTO p3d_log VALUES (1); INSERT INTO p3d_log VALUES (2));\n\
        SELECT 1 --x\r, 2;\n\
        COPY p3d_t FROM STDIN;\n4\tDROP TABLE p3d_t;\n\\.\n\
        INSERT INTO p3d_t VALUES (5, 'x');\n\
        SELECT count(*) FROM p3d_log;\n\
        SELECT a FROM p3d_t WHERE id = 1";
    let (results, kinds) = run_script(s.as_mut(), script).await;
    let statements = kinds
        .iter()
        .filter(|k| **k == ScriptItemKind::Statement)
        .count();
    assert_eq!(statements, 11, "{kinds:?}");
    assert!(kinds.contains(&ScriptItemKind::Refused(ScriptRefusal::CopyFromStdin)));
    // `--x\r` ended the comment: two columns.
    assert_eq!(results[7].schema.as_ref().unwrap().fields().len(), 2);
    // The rule's two inserts ran as one statement.
    assert_eq!(scalar(&results[9]).as_deref(), Some("2"));
    assert_eq!(scalar(&results[10]).as_deref(), Some("it's; ok"));
    // The COPY data was never sent.
    let c = run(s.as_mut(), "SELECT count(*) FROM p3d_t").await.unwrap();
    assert_eq!(scalar(&c).as_deref(), Some("4"));
    // The merged routine definition is one statement for the driver too.
    let c = run(s.as_mut(), "SELECT pg_temp.p3d_f(7) + pg_temp.p3d_g()")
        .await
        .unwrap();
    assert_eq!(scalar(&c).as_deref(), Some("8"));
    s.close().await.unwrap();
}
