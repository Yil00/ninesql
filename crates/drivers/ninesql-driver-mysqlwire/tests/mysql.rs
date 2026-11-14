//! Integration tests against the `mysql` and `mariadb` services of `dev/compose.yaml`
//! (ADR-0009). Every test runs on each engine that is reachable.
//!
//! An engine is skipped when unreachable, unless `NINESQL_REQUIRE_ENGINES` lists it. Start them
//! with `python3 dev/ready.py up core` (or `docker compose -f dev/compose.yaml up -d mysql
//! mariadb`). MySQL's `caching_sha2_password` needs the account's verifier in the server cache
//! for a plain-TCP connection: the service's healthcheck (a TLS client) fills it.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stderr)]

use std::path::PathBuf;
use std::time::Duration;

use ninesql_driver_api::arrow_array::cast::AsArray;
use ninesql_driver_api::arrow_array::types::{
    Date32Type, Float64Type, Int8Type, Int16Type, Int32Type, Int64Type, TimestampMicrosecondType,
    UInt32Type, UInt64Type,
};
use ninesql_driver_api::arrow_array::{Array, RecordBatch};
use ninesql_driver_api::arrow_schema::{DataType, TimeUnit};
use ninesql_driver_api::{
    Auth, CancelToken, ConnectParams, Driver, DriverError, ENGINE_TYPE_METADATA_KEY, Endpoint,
    EngineId, ExecOptions, ExplainFormat, FetchBudget, IsolationLevel, ObjectDetails, ObjectKind,
    ObjectPath, PageRequest, Param, PropertyValue, ResultItem, ResultSetKind, RowChange, RowEdit,
    Secret, Session, Statement, TRUNCATED_VALUES_METADATA_KEY, TlsMode, conformance,
};
use ninesql_driver_mysqlwire::MySqlWireDriver;

struct Dev {
    engine: EngineId,
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

/// Dev parameters of `id`, or `None` (skipped) when the engine is unreachable.
fn dev_of(id: &str, engine: EngineId) -> Option<Dev> {
    let doc = engines_json();
    let e = doc["engines"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == id)
        .unwrap();
    let dev = Dev {
        engine,
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
        .any(|s| s.trim() == id);
    assert!(!required, "{id} is required but unreachable at {addr}");
    eprintln!("skipped: {id} unreachable at {addr} (python3 dev/ready.py up core)");
    None
}

/// Every reachable engine of the family.
fn devs() -> Vec<Dev> {
    [
        dev_of("mysql", EngineId::MYSQL),
        dev_of("mariadb", EngineId::MARIADB),
    ]
    .into_iter()
    .flatten()
    .collect()
}

fn params(dev: &Dev) -> ConnectParams {
    let mut p = ConnectParams::new(dev.engine.clone(), Endpoint::tcp(&dev.host, dev.port));
    p.auth = Auth::password(&dev.user, Some(Secret::new(dev.password.clone())));
    p.database = Some(dev.database.clone());
    p.tls.mode = TlsMode::Disable;
    p.application_name = "ninesql-mysqlwire-tests".into();
    p
}

async fn connect(dev: &Dev) -> Box<dyn Session> {
    match MySqlWireDriver::new().connect(params(dev)).await {
        Ok(s) => s,
        Err(e) => panic!("{}: connect failed: {e}", dev.engine),
    }
}

#[derive(Default)]
struct Collected {
    schemas: Vec<ninesql_driver_api::arrow_schema::SchemaRef>,
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
                    c.schemas.push(schema);
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

/// First column of the first row as text.
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
        DataType::Int8 => col.as_primitive::<Int8Type>().value(0).to_string(),
        DataType::UInt64 => col.as_primitive::<UInt64Type>().value(0).to_string(),
        DataType::UInt32 => col.as_primitive::<UInt32Type>().value(0).to_string(),
        other => panic!("unexpected scalar type {other:?}"),
    })
}

/// `execute`, retrying while the session is still busy confirming a previous end.
async fn run_when_idle(s: &mut dyn Session, sql: &str) -> Result<Collected, DriverError> {
    for _ in 0..200 {
        match run(s, sql).await {
            Err(DriverError::Busy) => tokio::time::sleep(Duration::from_millis(50)).await,
            other => return other,
        }
    }
    Err(DriverError::Busy)
}

/// Test objects (idempotent): a table, a function that writes (invisible to the classifier),
/// a function that lifts the read-only mode, a procedure with two result sets.
async fn fixtures(dev: &Dev) {
    let mut s = connect(dev).await;
    for sql in [
        "CREATE TABLE IF NOT EXISTS ninesql_written (a INT)",
        "CREATE TABLE IF NOT EXISTS ninesql_edits (id INT PRIMARY KEY, name VARCHAR(50), \
         note TEXT)",
        "DROP FUNCTION IF EXISTS ninesql_write_one",
        "CREATE FUNCTION ninesql_write_one() RETURNS INT DETERMINISTIC MODIFIES SQL DATA \
         BEGIN INSERT INTO ninesql_written VALUES (1); RETURN 1; END",
        "DROP PROCEDURE IF EXISTS ninesql_two_sets",
        "CREATE PROCEDURE ninesql_two_sets() BEGIN SELECT 1 AS a; SELECT 'x' AS b, 2 AS c; END",
        "DROP PROCEDURE IF EXISTS ninesql_lift",
        "CREATE PROCEDURE ninesql_lift() SET SESSION TRANSACTION READ WRITE",
        // Lifts the read-only mode from a `SELECT` (classified `Read`).
        "DROP FUNCTION IF EXISTS ninesql_lift_fn",
        "CREATE FUNCTION ninesql_lift_fn() RETURNS INT DETERMINISTIC \
         BEGIN SET @@SESSION.transaction_read_only = 0; RETURN 1; END",
    ] {
        if let Err(e) = run(s.as_mut(), sql).await {
            panic!("{}: fixture `{sql}` failed: {e}", dev.engine);
        }
    }
    if dev.engine == EngineId::MARIADB {
        // MariaDB lets a stored function set the next transaction's access mode.
        for sql in [
            "DROP FUNCTION IF EXISTS ninesql_next_rw_fn",
            "CREATE FUNCTION ninesql_next_rw_fn() RETURNS INT DETERMINISTIC \
             BEGIN SET TRANSACTION READ WRITE; RETURN 1; END",
        ] {
            run(s.as_mut(), sql).await.unwrap();
        }
    }
    s.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connects_and_maps_types() {
    for dev in devs() {
        let mut s = connect(&dev).await;
        let v = s.server_version().unwrap().to_owned();
        if dev.engine == EngineId::MYSQL {
            assert!(v.starts_with("8.4"), "{v}");
        } else {
            assert!(v.starts_with("11.8") && v.contains("MariaDB"), "{v}");
        }
        assert_eq!(s.capabilities().server_version.as_deref(), Some(v.as_str()));
        let c = run(s.as_mut(), "SELECT * FROM all_types ORDER BY id")
            .await
            .unwrap();
        assert_eq!(c.rows(), 3);
        let schema = c.schemas[0].clone();
        let ty = |n: &str| schema.field_with_name(n).unwrap().data_type().clone();
        let meta = |n: &str| {
            schema
                .field_with_name(n)
                .unwrap()
                .metadata()
                .get(ENGINE_TYPE_METADATA_KEY)
                .cloned()
        };
        assert_eq!(ty("c_tinyint"), DataType::Int8);
        assert_eq!(ty("c_smallint"), DataType::Int16);
        assert_eq!(ty("c_mediumint"), DataType::Int32);
        assert_eq!(ty("c_int"), DataType::Int32);
        assert_eq!(ty("c_bigint"), DataType::Int64);
        assert_eq!(ty("c_bigint_u"), DataType::UInt64);
        assert_eq!(ty("c_float"), DataType::Float32);
        assert_eq!(ty("c_double"), DataType::Float64);
        assert_eq!(ty("c_varchar"), DataType::Utf8);
        assert_eq!(ty("c_text"), DataType::Utf8);
        assert_eq!(ty("c_varbinary"), DataType::Binary);
        assert_eq!(ty("c_blob"), DataType::Binary);
        assert_eq!(ty("c_date"), DataType::Date32);
        assert_eq!(
            ty("c_datetime"),
            DataType::Timestamp(TimeUnit::Microsecond, None)
        );
        for (col, name) in [
            ("c_decimal", "decimal"),
            ("c_time", "time"),
            ("c_enum", "enum"),
            ("c_set", "set"),
            ("c_year", "year"),
            ("c_timestamp", "timestamp"),
        ] {
            assert_eq!(meta(col).as_deref(), Some(name), "{} {col}", dev.engine);
        }
        // MariaDB reports JSON as LONGTEXT.
        if dev.engine == EngineId::MYSQL {
            assert_eq!(meta("c_json").as_deref(), Some("json"));
        }
        assert_eq!(meta("c_text"), None);
        let b = &c.batches[0];
        let col = |n: &str| b.column(schema.index_of(n).unwrap()).clone();
        assert_eq!(col("c_smallint").as_primitive::<Int16Type>().value(0), 42);
        assert_eq!(col("c_tinyint").as_primitive::<Int8Type>().value(1), -128);
        assert_eq!(
            col("c_bigint").as_primitive::<Int64Type>().value(1),
            i64::MAX
        );
        assert_eq!(
            col("c_bigint_u").as_primitive::<UInt64Type>().value(0),
            u64::MAX
        );
        assert_eq!(col("c_decimal").as_string::<i32>().value(0), "12345.678901");
        assert_eq!(
            col("c_text").as_string::<i32>().value(0),
            "text value with unicode: café, 日本, 🙂"
        );
        assert_eq!(
            col("c_binary").as_binary::<i32>().value(0),
            &[0xde, 0xad, 0xbe, 0xef]
        );
        assert_eq!(col("c_date").as_primitive::<Date32Type>().value(0), 20_771);
        assert_eq!(
            col("c_datetime")
                .as_primitive::<TimestampMicrosecondType>()
                .value(0),
            1_794_663_420_123_456
        );
        assert_eq!(
            col("c_time").as_string::<i32>().value(1),
            "-838:59:59.000000"
        );
        assert_eq!(
            col("c_double").as_primitive::<Float64Type>().value(1),
            -1e300
        );
        for i in 1..b.num_columns() {
            assert!(b.column(i).is_null(2), "{} row 3 is all NULL", dev.engine);
        }
        s.close().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conformance_reserved_properties_and_multiple_statements() {
    for dev in devs() {
        let driver = MySqlWireDriver::new();
        conformance::assert_reserved_properties_rejected(&driver, &params(&dev)).await;
        conformance::assert_multiple_statements_refused(&driver, params(&dev)).await;
        // The extra keys too, and unknown keys (allowlist), before any authentication.
        for key in [
            "characterEncoding",
            "rewriteBatchedStatements",
            "useCompression",
        ] {
            let mut p = params(&dev);
            p.auth = Auth::password("nobody", None);
            p.properties
                .insert(key.into(), PropertyValue::Plain("v-secret".into()));
            match driver.connect(p).await {
                Err(DriverError::InvalidParams(m)) => assert!(!m.contains("v-secret")),
                other => panic!("{key}: {:?}", other.map(|_| ())),
            }
        }
        // An accepted property reaches the server.
        let mut p = params(&dev);
        p.properties
            .insert("time_zone".into(), PropertyValue::Plain("+03:00".into()));
        let mut s = driver.connect(p).await.unwrap();
        let c = run(s.as_mut(), "SELECT @@SESSION.time_zone").await.unwrap();
        assert_eq!(scalar(&c).as_deref(), Some("+03:00"));
        // A statement split across a string that only the session's modes explain.
        assert!(
            run(s.as_mut(), "SELECT 'a\\'; SELECT 1; -- '")
                .await
                .is_ok()
        );
        s.close().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_only_refuses_writes_engine_side() {
    for dev in devs() {
        fixtures(&dev).await;
        let driver = MySqlWireDriver::new();
        let writes = conformance::writes_for(&dev.engine);
        conformance::assert_read_only_refuses_writes(&driver, params(&dev), &writes).await;

        // A write the classifier cannot see is rejected by the server (I3).
        let mut p = params(&dev);
        p.read_only = true;
        let mut s = driver.connect(p).await.unwrap();
        assert!(s.is_read_only());
        let before = run(s.as_mut(), "SELECT COUNT(*) FROM ninesql_written")
            .await
            .unwrap();
        match run(s.as_mut(), "SELECT ninesql_write_one()").await {
            Err(DriverError::ReadOnly(msg)) => assert!(msg.contains("1792"), "{msg}"),
            other => panic!(
                "{}: engine-side write not refused: {:?}",
                dev.engine,
                other.map(|_| ())
            ),
        }
        let after = run(s.as_mut(), "SELECT COUNT(*) FROM ninesql_written")
            .await
            .unwrap();
        assert_eq!(scalar(&before), scalar(&after));
        // Also inside a manual transaction.
        s.set_auto_commit(false).await.unwrap();
        assert!(!s.auto_commit());
        assert!(matches!(
            run(s.as_mut(), "SELECT ninesql_write_one()").await,
            Err(DriverError::ReadOnly(_))
        ));
        s.rollback().await.unwrap();
        // A procedure call is refused client-side (not classifiable).
        assert!(matches!(
            run(s.as_mut(), "CALL ninesql_lift()").await,
            Err(DriverError::ReadOnly(_))
        ));
        assert!(s.is_read_only());
        s.close().await.unwrap();
        // Read-only applied before init statements: an init write fails the connect.
        let mut p = params(&dev);
        p.read_only = true;
        p.init_statements = vec!["SELECT ninesql_write_one()".into()];
        assert!(matches!(
            driver.connect(p).await.map(|_| ()),
            Err(DriverError::ReadOnly(_))
        ));
        // Switching a session to read-only at runtime, and back.
        let mut s = connect(&dev).await;
        s.set_read_only(true).await.unwrap();
        assert!(matches!(
            run(s.as_mut(), "SELECT ninesql_write_one()").await,
            Err(DriverError::ReadOnly(_))
        ));
        s.set_read_only(false).await.unwrap();
        run(s.as_mut(), "SELECT ninesql_write_one()").await.unwrap();
        s.close().await.unwrap();
    }
}

/// Security review H1: `SET TRANSACTION … READ WRITE` (no SESSION) only changes the next
/// transaction and leaves `transaction_read_only` at 1, so the read-back cannot see it; the
/// comma form is classified `Session` by the shared classifier. The driver re-asserts the
/// session's read-only characteristic after every statement, so the next write still fails
/// engine-side.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_shot_read_write_transaction_is_reset() {
    const ONE_SHOT: &str = "SET TRANSACTION ISOLATION LEVEL READ COMMITTED, READ WRITE";
    for dev in devs() {
        fixtures(&dev).await;
        let mut p = params(&dev);
        p.read_only = true;
        let mut s = MySqlWireDriver::new().connect(p).await.unwrap();
        let count = |c: &Collected| scalar(c).unwrap();
        let before = count(
            &run(s.as_mut(), "SELECT COUNT(*) FROM ninesql_written")
                .await
                .unwrap(),
        );
        let mut lifts = vec![ONE_SHOT];
        if dev.engine == EngineId::MARIADB {
            lifts.push("SELECT ninesql_next_rw_fn()");
        }
        for manual in [false, true] {
            s.set_auto_commit(!manual).await.unwrap();
            for lift in &lifts {
                // Accepted (not classified as a lift), but it must not open a writable
                // transaction.
                run(s.as_mut(), lift).await.unwrap();
                match run(s.as_mut(), "SELECT ninesql_write_one()").await {
                    Err(DriverError::ReadOnly(_)) => {}
                    other => panic!(
                        "{} manual={manual} `{lift}`: write after a one-shot READ WRITE: {:?}",
                        dev.engine,
                        other.map(|_| ())
                    ),
                }
                if manual {
                    s.rollback().await.unwrap();
                }
                assert!(s.is_read_only());
            }
        }
        s.set_auto_commit(true).await.unwrap();
        let after = count(
            &run(s.as_mut(), "SELECT COUNT(*) FROM ninesql_written")
                .await
                .unwrap(),
        );
        assert_eq!(before, after, "{}: rows were written", dev.engine);
        s.close().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn access_mode_change_is_detected() {
    for dev in devs() {
        fixtures(&dev).await;
        let driver = MySqlWireDriver::new();
        // A lift the classifier cannot see: detected by the read-back, the session is closed.
        conformance::assert_access_mode_change_detected(
            &driver,
            params(&dev),
            "SELECT ninesql_lift_fn()",
        )
        .await;
        let mut p = params(&dev);
        p.read_only = true;
        let mut s = driver.connect(p).await.unwrap();
        // Statements that try to lift the mode are refused client-side.
        for sql in conformance::MYSQL_ACCESS_MODE_WRITES {
            assert!(
                matches!(run(s.as_mut(), sql).await, Err(DriverError::ReadOnly(_))),
                "{sql}"
            );
        }
        s.close().await.unwrap();
        // A character-set change closes any session (lexical mode).
        let mut s = connect(&dev).await;
        assert!(matches!(
            run(s.as_mut(), "SET NAMES latin1").await,
            Err(DriverError::Closed(_))
        ));
        assert!(matches!(
            run(s.as_mut(), "SELECT 1").await,
            Err(DriverError::Closed(_))
        ));
        // `sql_mode` changes are followed by the dialect.
        let mut s = connect(&dev).await;
        run(
            s.as_mut(),
            "SET SESSION sql_mode = 'ANSI_QUOTES,NO_BACKSLASH_ESCAPES'",
        )
        .await
        .unwrap();
        let c = run(s.as_mut(), "SELECT 'a\\' AS \"x\\\"").await.unwrap();
        assert_eq!(scalar(&c).as_deref(), Some("a\\"));
        assert_eq!(c.schemas[0].field(0).name(), "x\\");
        s.close().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn batches_stay_within_budget_on_a_million_rows() {
    for dev in devs() {
        let budget = FetchBudget::default()
            .with_batch_rows(5_000)
            .with_max_batch_bytes(512 * 1024)
            .with_max_value_bytes(40)
            .with_max_row_bytes(120)
            .with_max_bytes(1024 * 1024 * 1024);
        let report = conformance::assert_batches_within_budget(
            &MySqlWireDriver::new(),
            params(&dev),
            "SELECT id, event_time, kind, value, payload, REPEAT(kind, 30) AS wide FROM big_events",
            budget,
        )
        .await;
        assert_eq!(report.rows, 1_000_000);
        assert!(report.truncated_values);
        assert!(report.batches >= 200);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_values_are_cut_while_reading_and_budget_truncates() {
    for dev in devs() {
        let mut s = connect(&dev).await;
        // 8 MiB value (MySQL: 20 MiB, a row over several packets); the value budget is 1 MiB.
        let n = if dev.engine == EngineId::MYSQL {
            10 * 1024 * 1024
        } else {
            4 * 1024 * 1024
        };
        let c = run(
            s.as_mut(),
            &format!("SELECT REPEAT('é', {n}) AS big, 1 AS n"),
        )
        .await
        .unwrap();
        let b = &c.batches[0];
        let v = b.column(0).as_string::<i32>().value(0);
        assert!(v.len() <= 1024 * 1024 && v.len() > 1000, "{}", v.len());
        assert!(v.chars().all(|ch| ch == 'é'));
        assert!(
            b.schema()
                .field(0)
                .metadata()
                .contains_key(TRUNCATED_VALUES_METADATA_KEY)
        );
        // max_rows ends the result with Truncated; the session stays usable.
        let opts =
            ExecOptions::default().with_budget(FetchBudget::default().with_max_rows(Some(250)));
        let c = run_opts(s.as_mut(), Statement::sql("SELECT * FROM big_events"), opts)
            .await
            .unwrap();
        assert!(c.truncated);
        assert_eq!(c.rows(), 250);
        let c = run_when_idle(s.as_mut(), "SELECT 42").await.unwrap();
        assert_eq!(scalar(&c).as_deref(), Some("42"));
        s.close().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn identifier_quoting_round_trips() {
    for dev in devs() {
        conformance::assert_identifier_quoting(
            &MySqlWireDriver::new(),
            params(&dev),
            conformance::ALIAS_TEMPLATE,
            conformance::IDENTIFIER_SAMPLES,
        )
        .await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_cancel_is_ignored() {
    for dev in devs() {
        let rows = conformance::assert_stale_cancel_ignored(
            &MySqlWireDriver::new(),
            params(&dev),
            "SELECT * FROM big_events LIMIT 1000",
            "SELECT * FROM big_events LIMIT 100000",
        )
        .await;
        assert_eq!(rows, 100_000);
    }
}

async fn dev_ca(dev: &Dev) -> (tempfile::TempDir, PathBuf) {
    // The CA is generated in the container at first start (dev/tls/mysql-tls.sh).
    let mut s = connect(dev).await;
    let ca = scalar(&run(s.as_mut(), &dev.ca_sql).await.unwrap()).unwrap();
    assert!(ca.contains("BEGIN CERTIFICATE"));
    s.close().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ca.pem");
    std::fs::write(&path, ca).unwrap();
    (dir, path)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn verify_full_rejects_untrusted_certificates() {
    for dev in devs() {
        let (_dir, ca_path) = dev_ca(&dev).await;
        let make = || {
            let mut p = params(&dev);
            p.tls.mode = TlsMode::VerifyFull;
            p
        };
        conformance::assert_verify_full_rejects_untrusted_certificate(
            &MySqlWireDriver::new(),
            &make,
            &ca_path,
            "wrong.example.test",
        )
        .await;
        // `require` encrypts without verification; the session really is on TLS.
        let mut p = params(&dev);
        p.tls.mode = TlsMode::Require;
        let mut s = MySqlWireDriver::new().connect(p).await.unwrap();
        let c = run(s.as_mut(), "SHOW SESSION STATUS LIKE 'Ssl_cipher'")
            .await
            .unwrap();
        let cipher = c.batches[0]
            .column(1)
            .as_string::<i32>()
            .value(0)
            .to_owned();
        assert!(!cipher.is_empty(), "{}: not on TLS", dev.engine);
        s.close().await.unwrap();
        // A tunnel-style endpoint: connect to 127.0.0.1, verify the name "localhost"; cancel
        // (`KILL QUERY` on the control connection) travels over TLS as well.
        let mut p = params(&dev);
        p.tls.mode = TlsMode::VerifyFull;
        p.tls.ca_file = Some(ca_path.clone());
        p.tls.server_name = Some("localhost".into());
        let mut s = MySqlWireDriver::new().connect(p).await.unwrap();
        let opts = ExecOptions::default().with_timeout(Some(Duration::from_millis(500)));
        let started = std::time::Instant::now();
        assert!(matches!(
            run_opts(s.as_mut(), Statement::sql("SELECT SLEEP(30)"), opts).await,
            Err(DriverError::Timeout(_))
        ));
        assert!(started.elapsed() < Duration::from_secs(10));
        run_when_idle(s.as_mut(), "SELECT 1").await.unwrap();
        s.close().await.unwrap();
    }
}

/// `caching_sha2_password` (MySQL): without a cached verifier the server asks for the password
/// itself, which is refused over plain TCP and sent over verified TLS; afterwards the fast path
/// works over plain TCP.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_caching_sha2_authentication_needs_verified_tls() {
    let Some(dev) = dev_of("mysql", EngineId::MYSQL) else {
        return;
    };
    let (_dir, ca_path) = dev_ca(&dev).await;
    let root = |tls: bool| {
        let mut p = params(&dev);
        p.auth = Auth::password("root", Some(Secret::new("nine_dev_only_root".into())));
        if tls {
            p.tls.mode = TlsMode::VerifyFull;
            p.tls.ca_file = Some(ca_path.clone());
        }
        p
    };
    let driver = MySqlWireDriver::new();
    let mut admin = driver.connect(root(true)).await.unwrap();
    let user = format!("ninesql_fresh_{}", std::process::id());
    run(
        admin.as_mut(),
        &format!("CREATE USER '{user}'@'%' IDENTIFIED WITH caching_sha2_password BY 'fresh-Pa55'"),
    )
    .await
    .unwrap();
    let fresh = |tls: bool| {
        let mut p = root(tls);
        p.auth = Auth::password(&user, Some(Secret::new("fresh-Pa55".into())));
        p.database = None;
        p
    };
    match driver.connect(fresh(false)).await {
        Err(DriverError::Auth(m)) => {
            assert!(m.contains("verified TLS"), "{m}");
            assert!(!m.contains("fresh-Pa55"));
        }
        other => panic!(
            "full authentication over plain TCP: {:?}",
            other.map(|_| ())
        ),
    }
    driver
        .connect(fresh(true))
        .await
        .unwrap()
        .close()
        .await
        .unwrap();
    driver
        .connect(fresh(false))
        .await
        .unwrap()
        .close()
        .await
        .unwrap();
    run(admin.as_mut(), &format!("DROP USER '{user}'@'%'"))
        .await
        .unwrap();
    admin.close().await.unwrap();
}

/// A stream dropped mid-result: the driver kills the statement, drains it and frees the
/// session; nothing keeps running on the server.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropped_stream_frees_the_session() {
    for dev in devs() {
        let mut a = connect(&dev).await;
        let mut b = connect(&dev).await;
        let id = scalar(&run(a.as_mut(), "SELECT CONNECTION_ID()").await.unwrap()).unwrap();
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
        let started = std::time::Instant::now();
        let c = run_when_idle(a.as_mut(), "SELECT 1").await.unwrap();
        assert_eq!(scalar(&c).as_deref(), Some("1"));
        assert!(started.elapsed() < Duration::from_secs(6));
        let c = run(
            b.as_mut(),
            &format!(
                "SELECT COUNT(*) FROM information_schema.PROCESSLIST WHERE ID = {id} \
                 AND COMMAND = 'Query'"
            ),
        )
        .await
        .unwrap();
        assert_eq!(scalar(&c).as_deref(), Some("0"));
        assert!(!a.in_transaction());
        // A stream dropped while its statement runs on the server, before any row.
        let mut stream = a
            .execute(Statement::sql("SELECT SLEEP(30)"), ExecOptions::default())
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(300), stream.next())
                .await
                .is_err()
        );
        drop(stream);
        let started = std::time::Instant::now();
        let c = run_when_idle(a.as_mut(), "SELECT 3").await.unwrap();
        assert_eq!(scalar(&c).as_deref(), Some("3"));
        assert!(started.elapsed() < Duration::from_secs(5));
        a.close().await.unwrap();
        b.close().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_and_timeout_stop_running_statements() {
    for dev in devs() {
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
            Statement::sql("SELECT SLEEP(30)"),
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
            Statement::sql("SELECT SLEEP(30)"),
            ExecOptions::default().with_timeout(Some(Duration::from_millis(300))),
        )
        .await;
        assert!(matches!(r, Err(DriverError::Timeout(_))));
        run_when_idle(s.as_mut(), "SELECT 1").await.unwrap();
        // Session-level cancel handle while a statement runs.
        let mut stream = s
            .execute(Statement::sql("SELECT SLEEP(30)"), ExecOptions::default())
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
        let c = run_when_idle(s.as_mut(), "SELECT 5").await.unwrap();
        assert_eq!(scalar(&c).as_deref(), Some("5"));
        s.close().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transactions_savepoints_and_isolation() {
    for dev in devs() {
        fixtures(&dev).await;
        let mut s = connect(&dev).await;
        let mut other = connect(&dev).await;
        run(s.as_mut(), "DELETE FROM ninesql_edits WHERE id >= 100")
            .await
            .unwrap();
        assert!(s.auto_commit());
        s.set_auto_commit(false).await.unwrap();
        assert!(!s.auto_commit());
        run(s.as_mut(), "INSERT INTO ninesql_edits (id) VALUES (100)")
            .await
            .unwrap();
        assert!(s.in_transaction());
        s.savepoint("sp one").await.unwrap();
        run(s.as_mut(), "INSERT INTO ninesql_edits (id) VALUES (101)")
            .await
            .unwrap();
        s.rollback_to_savepoint("sp one").await.unwrap();
        // Not visible to another session before commit.
        let c = run(
            other.as_mut(),
            "SELECT COUNT(*) FROM ninesql_edits WHERE id >= 100",
        )
        .await
        .unwrap();
        assert_eq!(scalar(&c).as_deref(), Some("0"));
        s.commit().await.unwrap();
        assert!(!s.in_transaction());
        let c = run(
            other.as_mut(),
            "SELECT COUNT(*) FROM ninesql_edits WHERE id >= 100",
        )
        .await
        .unwrap();
        assert_eq!(scalar(&c).as_deref(), Some("1"));
        // Rollback, and switching back to auto-commit commits the open transaction.
        run(s.as_mut(), "INSERT INTO ninesql_edits (id) VALUES (102)")
            .await
            .unwrap();
        s.rollback().await.unwrap();
        run(s.as_mut(), "INSERT INTO ninesql_edits (id) VALUES (103)")
            .await
            .unwrap();
        s.set_auto_commit(true).await.unwrap();
        assert!(!s.in_transaction());
        let c = run(
            other.as_mut(),
            "SELECT GROUP_CONCAT(id ORDER BY id) FROM ninesql_edits WHERE id >= 100",
        )
        .await
        .unwrap();
        assert_eq!(scalar(&c).as_deref(), Some("100,103"));
        for level in [
            IsolationLevel::ReadUncommitted,
            IsolationLevel::ReadCommitted,
            IsolationLevel::RepeatableRead,
            IsolationLevel::Serializable,
        ] {
            s.set_isolation_level(level).await.unwrap();
        }
        let var = if dev.engine == EngineId::MYSQL {
            "@@SESSION.transaction_isolation"
        } else {
            "@@SESSION.tx_isolation"
        };
        let c = run(s.as_mut(), &format!("SELECT {var}")).await.unwrap();
        assert_eq!(scalar(&c).as_deref(), Some("SERIALIZABLE"));
        assert!(matches!(
            s.set_isolation_level(IsolationLevel::Snapshot).await,
            Err(DriverError::Unsupported(_))
        ));
        s.begin().await.unwrap();
        assert!(s.in_transaction());
        s.rollback().await.unwrap();
        s.ping().await.unwrap();
        s.close().await.unwrap();
        other.close().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edits_parameters_counts_notices_and_result_sets() {
    for dev in devs() {
        fixtures(&dev).await;
        let mut s = connect(&dev).await;
        run(s.as_mut(), "DELETE FROM ninesql_edits WHERE id < 100")
            .await
            .unwrap();
        let dialect = s.dialect();
        let table = vec![dev.database.clone(), "ninesql_edits".to_owned()];
        let tricky = "it's a \\ \"quote\" ; -- ? é".to_owned();
        let insert = dialect
            .render_edit(&RowEdit {
                table: table.clone(),
                change: RowChange::Insert {
                    values: vec![
                        ("id".into(), Param::Int(1)),
                        ("name".into(), Param::Text(tricky.clone())),
                        ("note".into(), Param::Null),
                    ],
                },
            })
            .unwrap();
        let c = run_opts(s.as_mut(), insert, ExecOptions::default())
            .await
            .unwrap();
        assert_eq!(c.counts, vec![Some(1)]);
        let update = dialect
            .render_edit(&RowEdit {
                table: table.clone(),
                change: RowChange::Update {
                    key: vec![("id".into(), Param::Int(1))],
                    values: vec![("note".into(), Param::Text("n".into()))],
                },
            })
            .unwrap();
        let c = run_opts(s.as_mut(), update, ExecOptions::default())
            .await
            .unwrap();
        assert_eq!(c.counts, vec![Some(1)]);
        let c = run_opts(
            s.as_mut(),
            Statement::sql("SELECT name, note FROM ninesql_edits WHERE id = ? AND name = ?")
                .with_params(vec![Param::Int(1), Param::Text(tricky.clone())]),
            ExecOptions::default(),
        )
        .await
        .unwrap();
        assert_eq!(scalar(&c).as_deref(), Some(tricky.as_str()));
        let delete = dialect
            .render_edit(&RowEdit {
                table,
                change: RowChange::Delete {
                    key: vec![("id".into(), Param::Int(1))],
                },
            })
            .unwrap();
        let c = run_opts(s.as_mut(), delete, ExecOptions::default())
            .await
            .unwrap();
        assert_eq!(c.counts, vec![Some(1)]);
        // Placeholders and parameters must match.
        assert!(matches!(
            run_opts(
                s.as_mut(),
                Statement::sql("SELECT ?").with_params(vec![Param::Int(1), Param::Int(2)]),
                ExecOptions::default()
            )
            .await,
            Err(DriverError::InvalidParams(_))
        ));
        // Warnings become notices.
        let c = run(s.as_mut(), "SELECT CAST('12abc' AS SIGNED) AS n")
            .await
            .unwrap();
        assert_eq!(scalar(&c).as_deref(), Some("12"));
        assert!(!c.notices.is_empty(), "{}: no warning notice", dev.engine);
        // Both result sets of a procedure, then its update count.
        let c = run(s.as_mut(), "CALL ninesql_two_sets()").await.unwrap();
        assert_eq!(c.schemas.len(), 2);
        assert_eq!(c.schemas[1].field(0).name(), "b");
        assert_eq!(c.rows(), 2);
        // Server errors keep their code.
        match run(s.as_mut(), "SELECT * FROM ninesql_no_such_table").await {
            Err(DriverError::Query { code, .. }) => assert_eq!(code.as_deref(), Some("1146")),
            other => panic!("{:?}", other.map(|_| ())),
        }
        // EXPLAIN in the declared formats.
        for format in s.capabilities().explain.clone() {
            let sql = dialect
                .explain("SELECT * FROM customers WHERE id = 1", format)
                .unwrap();
            let c = run(s.as_mut(), &sql).await.unwrap();
            assert!(c.rows() >= 1, "{format:?}");
        }
        assert!(
            dev.engine != EngineId::MARIADB
                || dialect
                    .explain("SELECT 1", ExplainFormat::Analyze)
                    .is_none()
        );
        // LOAD DATA LOCAL is never served.
        assert!(
            run(
                s.as_mut(),
                "LOAD DATA LOCAL INFILE '/etc/passwd' INTO TABLE ninesql_written"
            )
            .await
            .is_err()
        );
        run_when_idle(s.as_mut(), "SELECT 1").await.unwrap();
        s.close().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metadata_lists_with_filters_and_pages() {
    for dev in devs() {
        fixtures(&dev).await;
        let s = connect(&dev).await;
        let md = s.metadata();
        let root = ObjectPath::root();
        let page = md
            .list(
                &root,
                ObjectKind::Schema,
                PageRequest::first().with_filter(Some(dev.database.clone())),
            )
            .await
            .unwrap();
        assert!(page.items.iter().any(|i| i.name() == dev.database));
        let schema = root.child(ObjectKind::Schema, dev.database.clone());
        let tables = md
            .list(&schema, ObjectKind::Table, PageRequest::first())
            .await
            .unwrap();
        assert!(tables.items.iter().any(|i| i.name() == "big_events"));
        assert!(
            !tables
                .items
                .iter()
                .any(|i| i.name() == "customer_order_totals")
        );
        let views = md
            .list(&schema, ObjectKind::View, PageRequest::first())
            .await
            .unwrap();
        assert!(
            views
                .items
                .iter()
                .any(|i| i.name() == "customer_order_totals")
        );
        // Pages: keyset by name.
        let first = md
            .list(
                &schema,
                ObjectKind::Table,
                PageRequest::first().with_limit(2),
            )
            .await
            .unwrap();
        assert_eq!(first.items.len(), 2);
        let next = md
            .list(
                &schema,
                ObjectKind::Table,
                PageRequest::first()
                    .with_limit(2)
                    .with_cursor(first.next_cursor.clone()),
            )
            .await
            .unwrap();
        assert!(next.items[0].name() > first.items[1].name());
        // Filters are literal: `_` matches only itself.
        let f = md
            .list(
                &schema,
                ObjectKind::Table,
                PageRequest::first().with_filter(Some("big_".into())),
            )
            .await
            .unwrap();
        assert!(f.items.iter().all(|i| i.name().contains("big_")));
        let none = md
            .list(
                &schema,
                ObjectKind::Table,
                PageRequest::first().with_filter(Some("b%g' OR 1=1 -- ".into())),
            )
            .await
            .unwrap();
        assert!(none.items.is_empty());
        let table = schema.child(ObjectKind::Table, "all_types");
        let cols = md
            .list(&table, ObjectKind::Column, PageRequest::first())
            .await
            .unwrap();
        let ObjectDetails::Column(id) = &cols.items[0].details else {
            panic!("column details");
        };
        assert!(id.primary_key && !id.nullable && id.ordinal == 1);
        let cols2 = md
            .list(
                &table,
                ObjectKind::Column,
                PageRequest::first()
                    .with_limit(3)
                    .with_cursor(Some("3".into())),
            )
            .await
            .unwrap();
        assert_eq!(cols2.items[0].name(), "c_mediumint");
        let routines = md
            .list(&schema, ObjectKind::Routine, PageRequest::first())
            .await
            .unwrap();
        assert!(
            routines
                .items
                .iter()
                .any(|i| i.name() == "ninesql_write_one")
        );
        assert!(matches!(
            md.list(&schema, ObjectKind::Sequence, PageRequest::first())
                .await,
            Err(DriverError::Unsupported(_))
        ));
        s.close().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrong_password_is_an_auth_error_without_the_password() {
    for dev in devs() {
        let mut p = params(&dev);
        p.auth = Auth::password(&dev.user, Some(Secret::new("Wr0ng-pa55word".into())));
        match MySqlWireDriver::new().connect(p).await {
            Err(DriverError::Auth(m)) => assert!(!m.contains("Wr0ng-pa55word"), "{m}"),
            other => panic!("{}: {:?}", dev.engine, other.map(|_| ())),
        }
        // Wrong engine for the server: refused with a clear error.
        let mut p = params(&dev);
        p.engine = if dev.engine == EngineId::MYSQL {
            EngineId::MARIADB
        } else {
            EngineId::MYSQL
        };
        assert!(matches!(
            MySqlWireDriver::new().connect(p).await.map(|_| ()),
            Err(DriverError::Connect(_))
        ));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closed_session_refuses_work() {
    for dev in devs() {
        let mut s = connect(&dev).await;
        let id = scalar(&run(s.as_mut(), "SELECT CONNECTION_ID()").await.unwrap()).unwrap();
        let mut killer = connect(&dev).await;
        run(killer.as_mut(), &format!("KILL CONNECTION {id}"))
            .await
            .unwrap();
        killer.close().await.unwrap();
        let r = run(s.as_mut(), "SELECT 1").await;
        assert!(
            matches!(r, Err(DriverError::Closed(_))),
            "{:?}",
            r.map(|_| ())
        );
        assert!(matches!(
            run(s.as_mut(), "SELECT 1").await,
            Err(DriverError::Closed(_))
        ));
        s.close().await.unwrap();
    }
}
