//! Integration tests for the wire-compatible engines of the pgwire family (P4-C): the
//! `timescaledb`, `yugabytedb` and `questdb` services of `dev/compose.yaml` (ADR-0009).
//!
//! Each test is skipped when its engine is unreachable, unless `NINESQL_REQUIRE_ENGINES` lists
//! it. Start them with `python3 dev/ready.py up sql`.
//!
//! QuestDB asks for a cleartext password and has no TLS: the driver sends it only over a
//! protected transport. The tests connect as if through a host-key-verified SSH tunnel whose
//! local end is the loopback address the dev service listens on (what the core passes for a
//! real tunnel); the cleartext refusal itself is tested below and in the unit tests.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stderr)]

use std::path::PathBuf;
use std::time::Duration;

use ninesql_driver_api::arrow_array::cast::AsArray;
use ninesql_driver_api::arrow_array::types::{Int32Type, Int64Type};
use ninesql_driver_api::arrow_array::{Array, RecordBatch};
use ninesql_driver_api::arrow_schema::DataType;
use ninesql_driver_api::{
    Auth, CancelSupport, ConnectParams, Dialect, Driver, DriverError, Endpoint, EngineId,
    ExecOptions, ExplainFormat, FetchBudget, IsolationLevel, Metadata, ObjectDetails, ObjectInfo,
    ObjectKind, ObjectPath, PageRequest, ReadOnlyMechanism, ResultItem, Secret, Session, Statement,
    StatementClass, TlsMode, TunnelInfo, conformance,
};
use ninesql_driver_pgwire::{PgDialect, PgWireDriver};
use tokio::sync::OnceCell;

#[derive(Clone)]
struct Dev {
    engine: EngineId,
    host: String,
    port: u16,
    user: String,
    password: String,
    database: String,
}

fn engines_json() -> serde_json::Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../dev/engines.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// Dev parameters of engine `id`, or `None` (test skipped) when it is unreachable.
fn dev(id: &str) -> Option<Dev> {
    let doc = engines_json();
    let e = doc["engines"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == id)
        .unwrap();
    let dev = Dev {
        engine: EngineId::new(id),
        host: e["host"].as_str().unwrap().to_owned(),
        port: u16::try_from(e["port"].as_u64().unwrap()).unwrap(),
        user: e["user"].as_str().unwrap().to_owned(),
        password: e["password"].as_str().unwrap().to_owned(),
        database: e["database"].as_str().unwrap().to_owned(),
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
    eprintln!("skipped: {id} unreachable at {addr} (python3 dev/ready.py up sql)");
    None
}

fn params(dev: &Dev) -> ConnectParams {
    let mut p = ConnectParams::new(dev.engine.clone(), Endpoint::tcp(&dev.host, dev.port));
    p.auth = Auth::password(&dev.user, Some(Secret::new(dev.password.clone())));
    p.database = Some(dev.database.clone());
    p.tls.mode = TlsMode::Disable;
    p.application_name = "ninesql-pgwire-p4c-tests".into();
    p.connect_timeout = Duration::from_secs(30);
    if dev.engine == EngineId::QUESTDB {
        // See the module documentation: QuestDB sends its password in clear.
        p.tunnel = Some(TunnelInfo::new("ssh.dev.invalid", 22));
    }
    p
}

/// QuestDB's read-only user (`dev/compose.yaml`: `QDB_PG_READONLY_USER`).
fn questdb_ro(dev: &Dev) -> ConnectParams {
    let mut p = params(dev);
    p.auth = Auth::password("nine_ro", Some(Secret::new(dev.password.clone())));
    p
}

async fn connect(p: ConnectParams) -> Box<dyn Session> {
    PgWireDriver::new().connect(p).await.unwrap()
}

#[derive(Default)]
struct Collected {
    batches: Vec<RecordBatch>,
    counts: Vec<Option<u64>>,
}

impl Collected {
    fn rows(&self) -> usize {
        self.batches.iter().map(RecordBatch::num_rows).sum()
    }
}

async fn run_opts(
    s: &mut dyn Session,
    sql: &str,
    opts: ExecOptions,
) -> Result<Collected, DriverError> {
    let mut stream = s.execute(Statement::sql(sql), opts).await?;
    let mut c = Collected::default();
    while let Some(item) = stream.next().await {
        match item? {
            ResultItem::Rows(b) => c.batches.push(b),
            ResultItem::UpdateCount(n) => c.counts.push(n),
            _ => {}
        }
    }
    Ok(c)
}

async fn run(s: &mut dyn Session, sql: &str) -> Result<Collected, DriverError> {
    run_opts(s, sql, ExecOptions::default()).await
}

/// `execute`, retrying while the session is still confirming a previous end.
async fn run_when_idle(s: &mut dyn Session, sql: &str) -> Result<Collected, DriverError> {
    for _ in 0..200 {
        match run(s, sql).await {
            Err(DriverError::Busy) => tokio::time::sleep(Duration::from_millis(50)).await,
            other => return other,
        }
    }
    Err(DriverError::Busy)
}

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
        other => format!("{other:?}"),
    })
}

/// Test objects of TimescaleDB and YugabyteDB in schema `ninesql_p4c`, created once per
/// engine and test binary.
static TS_FIXTURES: OnceCell<()> = OnceCell::const_new();
static YB_FIXTURES: OnceCell<()> = OnceCell::const_new();

async fn fixtures(dev: &Dev) {
    let cell = if dev.engine == EngineId::TIMESCALEDB {
        &TS_FIXTURES
    } else {
        &YB_FIXTURES
    };
    cell.get_or_init(|| async {
        let mut s = connect(params(dev)).await;
        for sql in [
            "CREATE SCHEMA IF NOT EXISTS ninesql_p4c",
            "CREATE TABLE IF NOT EXISTS ninesql_p4c.written (a int)",
            // A write the client-side classifier cannot see (a user function).
            "CREATE OR REPLACE FUNCTION ninesql_p4c.write_one() RETURNS int LANGUAGE sql \
             AS $$ INSERT INTO ninesql_p4c.written VALUES (1) RETURNING 1 $$",
            // Lifts the read-only mode without being classified as such.
            "CREATE OR REPLACE FUNCTION ninesql_p4c.lift_read_only() RETURNS text \
             LANGUAGE sql AS $$ SELECT set_config('default_transaction_read_only', 'off', false) $$",
        ] {
            run(s.as_mut(), sql).await.unwrap();
        }
        s.close().await.unwrap();
    })
    .await;
}

fn pg_engines() -> Vec<Dev> {
    ["timescaledb", "yugabytedb"]
        .into_iter()
        .filter_map(dev)
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Engine checks at connect
// ---------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_engine_connects_and_a_wrong_engine_is_refused() {
    let driver = PgWireDriver::new();
    for id in ["timescaledb", "yugabytedb", "questdb"] {
        let Some(dev) = dev(id) else { continue };
        let s = connect(params(&dev)).await;
        let caps = s.capabilities().clone();
        assert_eq!(caps.engine, dev.engine);
        let version = s.server_version().unwrap_or_default().to_owned();
        match id {
            "timescaledb" => assert!(
                caps.server_version
                    .as_deref()
                    .unwrap()
                    .contains("TimescaleDB 2."),
                "{caps:?}"
            ),
            "yugabytedb" => assert!(version.contains("-YB-"), "{version}"),
            _ => assert_eq!(caps.cancel, CancelSupport::ServerSide),
        }
        s.close().await.unwrap();
        // Every other engine id of the family is refused against this server, except
        // `postgresql` on TimescaleDB (a PostgreSQL database with an extension).
        for other in [
            EngineId::POSTGRESQL,
            EngineId::TIMESCALEDB,
            EngineId::YUGABYTEDB,
            EngineId::QUESTDB,
        ] {
            if other == dev.engine {
                continue;
            }
            let mut p = params(&dev);
            p.engine = other.clone();
            if other == EngineId::QUESTDB {
                p.properties.clear();
            }
            let outcome = driver.connect(p).await;
            if id == "timescaledb" && other == EngineId::POSTGRESQL {
                outcome.unwrap().close().await.unwrap();
                continue;
            }
            match outcome {
                Err(DriverError::Connect(m)) => {
                    assert!(m.contains("choose") && !m.contains(&dev.password), "{m}");
                }
                Err(e) => panic!("{id} as {other}: unexpected error {e:?}"),
                Ok(_) => panic!("{id} accepted as {other}"),
            }
        }
    }
    // A TimescaleDB connection to a database without the extension is refused: the
    // PostgreSQL service has none.
    if let Some(pg) = dev_postgres() {
        let mut p = params(&pg);
        p.engine = EngineId::TIMESCALEDB;
        match driver.connect(p).await {
            Err(DriverError::Connect(m)) => assert!(m.contains("timescaledb extension"), "{m}"),
            other => panic!("expected a refusal: {:?}", other.map(|_| ())),
        }
    }
}

/// The `postgresql` service, when reachable (not required by these tests).
fn dev_postgres() -> Option<Dev> {
    let doc = engines_json();
    let e = doc["engines"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == "postgresql")
        .unwrap();
    let dev = Dev {
        engine: EngineId::POSTGRESQL,
        host: e["host"].as_str().unwrap().to_owned(),
        port: u16::try_from(e["port"].as_u64().unwrap()).unwrap(),
        user: e["user"].as_str().unwrap().to_owned(),
        password: e["password"].as_str().unwrap().to_owned(),
        database: e["database"].as_str().unwrap().to_owned(),
    };
    let addr = format!("{}:{}", dev.host, dev.port);
    addr.parse()
        .ok()
        .and_then(|a| std::net::TcpStream::connect_timeout(&a, Duration::from_secs(1)).ok())
        .map(|_| dev)
}

// ---------------------------------------------------------------------------------------------
// Read-only (I3)
// ---------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timescaledb_and_yugabytedb_refuse_writes_engine_side() {
    let driver = PgWireDriver::new();
    for dev in pg_engines() {
        fixtures(&dev).await;
        let caps = driver.capabilities(&dev.engine, None).unwrap();
        assert_eq!(
            caps.read_only,
            ReadOnlyMechanism::SessionSetting { liftable: true }
        );
        let writes = conformance::writes_for(&dev.engine);
        conformance::assert_read_only_refuses_writes(&driver, params(&dev), &writes).await;
        // A write the classifier cannot see is rejected by the server.
        let mut p = params(&dev);
        p.read_only = true;
        let mut s = driver.connect(p).await.unwrap();
        let before = run(s.as_mut(), "SELECT count(*) FROM ninesql_p4c.written")
            .await
            .unwrap();
        match run(s.as_mut(), "SELECT ninesql_p4c.write_one()").await {
            Err(DriverError::ReadOnly(msg)) => assert!(msg.contains("read-only"), "{msg}"),
            other => panic!(
                "{}: engine-side write not refused: {:?}",
                dev.engine,
                other.map(|_| ())
            ),
        }
        let after = run(s.as_mut(), "SELECT count(*) FROM ninesql_p4c.written")
            .await
            .unwrap();
        assert_eq!(scalar(&before), scalar(&after));
        // Also inside a manual transaction.
        s.set_auto_commit(false).await.unwrap();
        assert!(matches!(
            run(s.as_mut(), "SELECT ninesql_p4c.write_one()").await,
            Err(DriverError::ReadOnly(_))
        ));
        s.rollback().await.unwrap();
        s.close().await.unwrap();
        // TimescaleDB's data-changing functions are refused client-side, and engine-side too.
        if dev.engine == EngineId::TIMESCALEDB {
            let mut p = params(&dev);
            p.read_only = true;
            let mut s = driver.connect(p).await.unwrap();
            let drop_chunks = "SELECT drop_chunks('public.sensor_readings', \
                               older_than => TIMESTAMPTZ '2000-01-01')";
            assert!(matches!(
                run(s.as_mut(), drop_chunks).await,
                Err(DriverError::ReadOnly(_))
            ));
            s.close().await.unwrap();
        }
        // The lift through a function is detected and closes the session.
        conformance::assert_access_mode_change_detected(
            &driver,
            params(&dev),
            "SELECT ninesql_p4c.lift_read_only()",
        )
        .await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn questdb_read_only_needs_its_read_only_user() {
    let Some(dev) = dev("questdb") else { return };
    let driver = PgWireDriver::new();
    let caps = driver.capabilities(&EngineId::QUESTDB, None).unwrap();
    assert_eq!(
        caps.read_only,
        ReadOnlyMechanism::ServerSetting { liftable: false }
    );
    // A user that may write: a read-only session is refused, never faked.
    let mut p = params(&dev);
    p.read_only = true;
    match driver.connect(p).await {
        Err(DriverError::ReadOnly(m)) => assert!(m.contains("pg.readonly.user"), "{m}"),
        other => panic!("read-only session as a writer: {:?}", other.map(|_| ())),
    }
    // The read-only user: accepted, and the conformance writes are refused.
    let writes = conformance::writes_for(&EngineId::QUESTDB);
    conformance::assert_read_only_refuses_writes(&driver, questdb_ro(&dev), &writes).await;
    let mut p = questdb_ro(&dev);
    p.read_only = true;
    let mut s = driver.connect(p).await.unwrap();
    assert!(s.is_read_only());
    assert!(run(s.as_mut(), "SELECT count() FROM orders").await.is_ok());
    // Not liftable from the session.
    assert!(matches!(
        s.set_read_only(false).await,
        Err(DriverError::ReadOnly(_))
    ));
    s.close().await.unwrap();
    // Engine-side: the same account in a session not marked read-only (so the client-side
    // classifier lets writes through) cannot write either.
    let mut s = connect(questdb_ro(&dev)).await;
    for sql in [
        "INSERT INTO nine_meta VALUES ('p4c', 'x')",
        "CREATE TABLE ninesql_p4c_ro (a INT)",
        "UPDATE nine_meta SET value = 'x'",
        "TRUNCATE TABLE nine_meta",
    ] {
        match run(s.as_mut(), sql).await {
            Err(DriverError::Query { message, .. }) => {
                assert!(message.contains("permission denied"), "{sql}: {message}");
            }
            other => panic!("{sql}: not refused engine-side: {:?}", other.map(|_| ())),
        }
    }
    let meta = run(s.as_mut(), "SELECT count() FROM nine_meta")
        .await
        .unwrap();
    assert_eq!(scalar(&meta).as_deref(), Some("2"));
    s.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn questdb_password_needs_a_protected_transport() {
    let Some(dev) = dev("questdb") else { return };
    // Plain TCP without a verified tunnel: QuestDB asks for a cleartext password, refused
    // before the password is sent.
    let mut p = params(&dev);
    p.tunnel = None;
    match PgWireDriver::new().connect(p).await {
        Err(DriverError::Auth(m)) => assert!(!m.contains(&dev.password), "{m}"),
        other => panic!("cleartext password sent: {:?}", other.map(|_| ())),
    }
}

// ---------------------------------------------------------------------------------------------
// Statements, budgets, cancel
// ---------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn batches_stay_within_budget_on_each_engine() {
    for id in ["timescaledb", "yugabytedb", "questdb"] {
        let Some(dev) = dev(id) else { continue };
        let budget = FetchBudget::default()
            .with_batch_rows(5_000)
            .with_max_batch_bytes(512 * 1024)
            .with_max_value_bytes(40)
            .with_max_row_bytes(160)
            .with_max_bytes(1024 * 1024 * 1024);
        let sql = if id == "questdb" {
            "SELECT id, event_time, kind, value, payload, concat(payload, payload, payload, \
             payload, payload) AS wide FROM big_events"
        } else {
            "SELECT id, event_time, kind, value, payload, repeat(kind, 30) AS wide FROM big_events"
        };
        let report = conformance::assert_batches_within_budget(
            &PgWireDriver::new(),
            params(&dev),
            sql,
            budget,
        )
        .await;
        assert_eq!(report.rows, 1_000_000, "{id}");
        assert!(report.truncated_values, "{id}");
        assert!(report.batches >= 200, "{id}");
        conformance::assert_multiple_statements_refused(&PgWireDriver::new(), params(&dev)).await;
        conformance::assert_reserved_properties_rejected(&PgWireDriver::new(), &params(&dev)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_timeout_and_drop_stop_statements() {
    for id in ["timescaledb", "yugabytedb", "questdb"] {
        let Some(dev) = dev(id) else { continue };
        let mut s = connect(params(&dev)).await;
        // A long statement stopped by the timeout (server-side cancel).
        let long = if id == "questdb" {
            "SELECT count() FROM long_sequence(100000000000) WHERE rnd_double() > 2"
        } else {
            "SELECT pg_sleep(60)"
        };
        let started = std::time::Instant::now();
        let outcome = run_opts(
            s.as_mut(),
            long,
            ExecOptions::default().with_timeout(Some(Duration::from_secs(2))),
        )
        .await;
        assert!(
            matches!(outcome, Err(DriverError::Timeout(_))),
            "{id}: {:?}",
            outcome.map(|_| ())
        );
        assert!(started.elapsed() < Duration::from_secs(20), "{id}");
        let one = run_when_idle(s.as_mut(), "SELECT 1").await.unwrap();
        assert_eq!(one.rows(), 1, "{id}");
        // A stream dropped after its first batch: the session stays usable.
        {
            let mut stream = s
                .execute(
                    Statement::sql("SELECT * FROM big_events"),
                    ExecOptions::default().with_budget(FetchBudget::default().with_batch_rows(100)),
                )
                .await
                .unwrap();
            let mut seen = 0;
            while let Some(item) = stream.next().await {
                if let ResultItem::Rows(b) = item.unwrap() {
                    seen += b.num_rows();
                    break;
                }
            }
            assert!(seen > 0);
        }
        let n = run_when_idle(s.as_mut(), "SELECT count(*) FROM products")
            .await
            .unwrap();
        assert_eq!(scalar(&n).as_deref(), Some("100"), "{id}");
        s.close().await.unwrap();
        let first = if id == "questdb" {
            "SELECT count() FROM long_sequence(1000)"
        } else {
            "SELECT pg_sleep(0.2)"
        };
        conformance::assert_stale_cancel_ignored(
            &PgWireDriver::new(),
            params(&dev),
            first,
            "SELECT count(*) FROM products",
        )
        .await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transactions_follow_the_declared_capabilities() {
    if let Some(dev) = dev("yugabytedb") {
        fixtures(&dev).await;
        let mut s = connect(params(&dev)).await;
        let levels = s.capabilities().transactions.isolation_levels.clone();
        assert!(levels.contains(&IsolationLevel::RepeatableRead));
        for level in levels {
            s.set_isolation_level(level).await.unwrap();
        }
        assert!(
            s.set_isolation_level(IsolationLevel::ReadUncommitted)
                .await
                .is_err()
        );
        s.set_auto_commit(false).await.unwrap();
        run(s.as_mut(), "INSERT INTO ninesql_p4c.written VALUES (42)")
            .await
            .unwrap();
        s.savepoint("sp1").await.unwrap();
        run(s.as_mut(), "INSERT INTO ninesql_p4c.written VALUES (43)")
            .await
            .unwrap();
        s.rollback_to_savepoint("sp1").await.unwrap();
        assert!(s.in_transaction());
        s.rollback().await.unwrap();
        let c = run(
            s.as_mut(),
            "SELECT count(*) FROM ninesql_p4c.written WHERE a IN (42, 43)",
        )
        .await
        .unwrap();
        assert_eq!(scalar(&c).as_deref(), Some("0"));
        s.close().await.unwrap();
    }
    if let Some(dev) = dev("questdb") {
        let mut s = connect(params(&dev)).await;
        let caps = s.capabilities().clone();
        assert!(!caps.transactions.manual_commit);
        assert!(!s.auto_commit());
        assert!(matches!(
            s.set_auto_commit(false).await,
            Err(DriverError::Unsupported(_))
        ));
        assert!(matches!(s.begin().await, Err(DriverError::Unsupported(_))));
        assert!(matches!(
            s.savepoint("x").await,
            Err(DriverError::Unsupported(_))
        ));
        for sql in ["BEGIN", "COMMIT", "ROLLBACK", "START TRANSACTION"] {
            assert!(
                matches!(run(s.as_mut(), sql).await, Err(DriverError::Unsupported(_))),
                "{sql}"
            );
        }
        // Commit and rollback with nothing open do nothing.
        s.commit().await.unwrap();
        s.rollback().await.unwrap();
        s.close().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explain_formats_run_and_types_map() {
    for id in ["timescaledb", "yugabytedb", "questdb"] {
        let Some(dev) = dev(id) else { continue };
        let mut s = connect(params(&dev)).await;
        let dialect = s.dialect();
        let explain = s.capabilities().explain.clone();
        for format in [
            ExplainFormat::Text,
            ExplainFormat::Json,
            ExplainFormat::Analyze,
        ] {
            let sql = dialect.explain("SELECT * FROM orders WHERE id = 1", format);
            assert_eq!(sql.is_some(), explain.contains(&format), "{id} {format:?}");
            if let Some(sql) = sql {
                let c = run(s.as_mut(), &sql).await.unwrap();
                assert!(c.rows() > 0, "{id} {format:?}");
            }
        }
        // The "nine" all_types table decodes (row 3 is all NULLs).
        let c = run(s.as_mut(), "SELECT * FROM all_types ORDER BY id")
            .await
            .unwrap();
        assert_eq!(c.rows(), 3, "{id}");
        // Bound parameters.
        let mut stmt = Statement::sql("SELECT count(*) FROM orders WHERE id <= $1");
        stmt.params = vec![ninesql_driver_api::Param::Int(10)];
        let mut stream = s.execute(stmt, ExecOptions::default()).await.unwrap();
        let mut got = Collected::default();
        while let Some(item) = stream.next().await {
            if let ResultItem::Rows(b) = item.unwrap() {
                got.batches.push(b);
            }
        }
        assert_eq!(scalar(&got).as_deref(), Some("10"), "{id}");
        s.close().await.unwrap();
    }
}

// ---------------------------------------------------------------------------------------------
// Metadata: the "nine" dataset browses fully, on read-only sessions
// ---------------------------------------------------------------------------------------------

async fn all(m: &dyn Metadata, parent: &ObjectPath, kind: ObjectKind) -> Vec<ObjectInfo> {
    let mut out = Vec::new();
    let mut page = PageRequest::first();
    page.limit = 3;
    loop {
        let p = m.list(parent, kind, page.clone()).await.unwrap();
        out.extend(p.items);
        match p.next_cursor {
            Some(c) => page.cursor = Some(c),
            None => break,
        }
        assert!(out.len() < 100_000);
    }
    out
}

fn names(items: &[ObjectInfo]) -> Vec<String> {
    items
        .iter()
        .map(|i| i.path.leaf().unwrap().name.clone())
        .collect()
}

const NINE_TABLES: &[&str] = &[
    "all_types",
    "big_events",
    "customers",
    "nine_meta",
    "orders",
    "products",
];

/// Lists every child kind of every object of the "nine" dataset and generates every DDL the
/// capabilities declare. Returns how many objects were visited.
async fn browse(
    m: &dyn Metadata,
    caps: &ninesql_driver_api::Capabilities,
    root: ObjectPath,
) -> usize {
    let mut visited = 0;
    let mut stack = vec![root];
    while let Some(parent) = stack.pop() {
        for kind in m.child_kinds(parent.kind()) {
            assert!(caps.metadata.kinds.contains(&kind), "{kind:?} not declared");
            for item in all(m, &parent, kind).await {
                visited += 1;
                if caps.metadata.ddl.contains(&kind) {
                    let ddl = m.ddl(&item.path).await;
                    let skip =
                        matches!(&item.details, ObjectDetails::Type(t) if t.flavour == "base");
                    if !skip {
                        let ddl = ddl.unwrap_or_else(|e| panic!("DDL of {:?}: {e}", item.path));
                        assert!(!ddl.is_empty());
                    }
                }
                if !m.child_kinds(Some(kind)).is_empty() {
                    stack.push(item.path.clone());
                }
            }
        }
    }
    visited
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nine_dataset_browses_fully_on_timescaledb_and_yugabytedb() {
    for dev in pg_engines() {
        let mut p = params(&dev);
        p.read_only = true;
        let s = connect(p).await;
        let caps = s.capabilities().clone();
        let m = s.metadata();
        let schemas = names(&all(m.as_ref(), &ObjectPath::root(), ObjectKind::Schema).await);
        assert!(schemas.contains(&"public".to_owned()), "{schemas:?}");
        let public = ObjectPath::root().child(ObjectKind::Schema, "public");
        let tables = all(m.as_ref(), &public, ObjectKind::Table).await;
        let table_names = names(&tables);
        for t in NINE_TABLES {
            assert!(
                table_names.contains(&(*t).to_owned()),
                "{}: {table_names:?}",
                dev.engine
            );
        }
        let views = names(&all(m.as_ref(), &public, ObjectKind::View).await);
        assert!(
            views.contains(&"customer_order_totals".to_owned()),
            "{views:?}"
        );
        let orders = public.child(ObjectKind::Table, "orders");
        let fks = all(m.as_ref(), &orders, ObjectKind::ForeignKey).await;
        assert_eq!(fks.len(), 2, "{}", dev.engine);
        let indexes = all(m.as_ref(), &orders, ObjectKind::Index).await;
        assert!(indexes.len() >= 2, "{}", dev.engine);
        let columns = all(m.as_ref(), &orders, ObjectKind::Column).await;
        assert_eq!(columns.len(), 6, "{}", dev.engine);
        let ddl = m.ddl(&orders).await.unwrap();
        assert!(ddl.starts_with("CREATE TABLE"), "{ddl}");
        if dev.engine == EngineId::TIMESCALEDB {
            // The hypertable is a table, flagged; its chunks are not listed anywhere.
            let ht = tables
                .iter()
                .find(|t| t.path.leaf().unwrap().name == "sensor_readings")
                .unwrap();
            assert!(
                matches!(&ht.details, ObjectDetails::Table(t) if t.flavour.as_deref() == Some("hypertable"))
            );
            let internal = ObjectPath::root().child(ObjectKind::Schema, "_timescaledb_internal");
            let chunks: Vec<String> = names(&all(m.as_ref(), &internal, ObjectKind::Table).await)
                .into_iter()
                .filter(|n| n.starts_with("_hyper_") || n.starts_with("compress_hyper_"))
                .collect();
            assert!(chunks.is_empty(), "{chunks:?}");
            let caggs = all(m.as_ref(), &public, ObjectKind::View).await;
            let cagg = caggs
                .iter()
                .find(|v| v.path.leaf().unwrap().name == "sensor_readings_hourly")
                .unwrap();
            assert!(
                matches!(&cagg.details, ObjectDetails::Table(t) if t.flavour.as_deref() == Some("continuous aggregate"))
            );
            assert!(
                m.ddl(&cagg.path)
                    .await
                    .unwrap()
                    .starts_with("CREATE OR REPLACE VIEW")
            );
        }
        let visited = browse(m.as_ref(), &caps, public).await;
        assert!(visited > 50, "{}: {visited}", dev.engine);
        assert!(s.is_read_only());
        s.close().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nine_dataset_browses_fully_on_questdb() {
    let Some(dev) = dev("questdb") else { return };
    let mut p = questdb_ro(&dev);
    p.read_only = true;
    let s = connect(p).await;
    let caps = s.capabilities().clone();
    let m = s.metadata();
    assert_eq!(
        m.child_kinds(None),
        vec![
            ObjectKind::Table,
            ObjectKind::View,
            ObjectKind::MaterializedView
        ]
    );
    let tables = all(m.as_ref(), &ObjectPath::root(), ObjectKind::Table).await;
    let table_names = names(&tables);
    for t in NINE_TABLES {
        assert!(table_names.contains(&(*t).to_owned()), "{table_names:?}");
    }
    let mut sorted = table_names.clone();
    sorted.sort();
    assert_eq!(sorted, table_names, "keyset pages stay in order");
    let orders_info = tables
        .iter()
        .find(|t| t.path.leaf().unwrap().name == "orders")
        .unwrap();
    match &orders_info.details {
        ObjectDetails::Table(t) => {
            assert_eq!(t.estimated_rows, Some(10_000));
            assert_eq!(t.partition_key.as_deref(), Some("ordered_at BY MONTH"));
        }
        other => panic!("{other:?}"),
    }
    let orders = ObjectPath::root().child(ObjectKind::Table, "orders");
    let columns = all(m.as_ref(), &orders, ObjectKind::Column).await;
    assert_eq!(
        names(&columns),
        [
            "id",
            "customer_id",
            "product_id",
            "quantity",
            "status",
            "ordered_at"
        ]
    );
    match &columns[4].details {
        ObjectDetails::Column(c) => {
            assert_eq!(c.data_type, "SYMBOL");
            assert_eq!(c.ordinal, 5);
        }
        other => panic!("{other:?}"),
    }
    let partitions = all(m.as_ref(), &orders, ObjectKind::Partition).await;
    assert!(partitions.len() >= 2, "{}", partitions.len());
    let rows: u64 = partitions
        .iter()
        .map(|p| match &p.details {
            ObjectDetails::Partition(p) => p.estimated_rows.unwrap_or(0),
            _ => 0,
        })
        .sum();
    assert_eq!(rows, 10_000);
    // A table not partitioned has no partitions.
    let products = ObjectPath::root().child(ObjectKind::Table, "products");
    assert!(
        all(m.as_ref(), &products, ObjectKind::Partition)
            .await
            .is_empty()
    );
    // Filters are literal substrings, bound.
    let mut page = PageRequest::first();
    page.filter = Some("order".into());
    let p = m
        .list(&ObjectPath::root(), ObjectKind::Table, page)
        .await
        .unwrap();
    assert_eq!(names(&p.items), ["orders"]);
    let mut page = PageRequest::first();
    page.filter = Some("' OR '1'='1".into());
    let p = m
        .list(&ObjectPath::root(), ObjectKind::Table, page)
        .await
        .unwrap();
    assert!(p.items.is_empty());
    let mut page = PageRequest::first();
    page.filter = Some("_id".into());
    let p = m.list(&orders, ObjectKind::Column, page).await.unwrap();
    assert_eq!(names(&p.items), ["customer_id", "product_id"]);
    // DDL.
    let ddl = m.ddl(&orders).await.unwrap();
    assert!(
        ddl.starts_with("CREATE TABLE") && ddl.contains("PARTITION BY MONTH"),
        "{ddl}"
    );
    let missing = ObjectPath::root().child(ObjectKind::Table, "no_such\"table");
    assert!(m.ddl(&missing).await.is_err());
    let visited = browse(m.as_ref(), &caps, ObjectPath::root()).await;
    assert!(visited > 40, "{visited}");
    s.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn questdb_views_and_materialized_views_are_listed() {
    let Some(dev) = dev("questdb") else { return };
    let mut w = connect(params(&dev)).await;
    for sql in [
        "DROP VIEW IF EXISTS ninesql_p4c_v",
        "DROP MATERIALIZED VIEW IF EXISTS ninesql_p4c_mv",
        "CREATE VIEW ninesql_p4c_v AS (SELECT id, sku FROM products)",
        "CREATE MATERIALIZED VIEW ninesql_p4c_mv AS (SELECT ordered_at, count() c FROM orders \
         SAMPLE BY 1d) PARTITION BY MONTH",
    ] {
        run(w.as_mut(), sql).await.unwrap();
    }
    let m = w.metadata();
    let views = names(&all(m.as_ref(), &ObjectPath::root(), ObjectKind::View).await);
    assert!(views.contains(&"ninesql_p4c_v".to_owned()), "{views:?}");
    let mvs = names(
        &all(
            m.as_ref(),
            &ObjectPath::root(),
            ObjectKind::MaterializedView,
        )
        .await,
    );
    assert!(mvs.contains(&"ninesql_p4c_mv".to_owned()), "{mvs:?}");
    let v = ObjectPath::root().child(ObjectKind::View, "ninesql_p4c_v");
    assert_eq!(
        names(&all(m.as_ref(), &v, ObjectKind::Column).await),
        ["id", "sku"]
    );
    assert!(m.ddl(&v).await.unwrap().starts_with("CREATE VIEW"));
    let mv = ObjectPath::root().child(ObjectKind::MaterializedView, "ninesql_p4c_mv");
    assert!(
        m.ddl(&mv)
            .await
            .unwrap()
            .starts_with("CREATE MATERIALIZED VIEW")
    );
    // A view is not a table.
    let as_table = ObjectPath::root().child(ObjectKind::Table, "ninesql_p4c_v");
    assert!(m.ddl(&as_table).await.is_err());
    for sql in [
        "DROP VIEW ninesql_p4c_v",
        "DROP MATERIALIZED VIEW ninesql_p4c_mv",
    ] {
        run(w.as_mut(), sql).await.unwrap();
    }
    w.close().await.unwrap();
}

// ---------------------------------------------------------------------------------------------
// Dialect overrides (no server)
// ---------------------------------------------------------------------------------------------

#[test]
fn engine_dialects_classify_their_own_statements() {
    let ts = PgDialect::timescaledb();
    let q = PgDialect::questdb();
    let yb = PgDialect::yugabytedb();
    let pg = PgDialect::new();
    for (sql, class) in [
        (
            "SELECT drop_chunks('sensor_readings', older_than => now() - interval '1 day')",
            StatementClass::Unknown,
        ),
        (
            "SELECT public.compress_chunk(c) FROM show_chunks('t') c",
            StatementClass::Unknown,
        ),
        (
            "SELECT \"drop_chunks\"('t', INTERVAL '1 day')",
            StatementClass::Unknown,
        ),
        (
            "CALL refresh_continuous_aggregate('v', NULL, NULL)",
            StatementClass::Unknown,
        ),
        (
            "SELECT create_hypertable('t', by_range('time'))",
            StatementClass::Unknown,
        ),
        (
            "SELECT * FROM timescaledb_information.chunks",
            StatementClass::Read,
        ),
        (
            "SELECT time_bucket('1 hour', time), avg(v) FROM t GROUP BY 1",
            StatementClass::Read,
        ),
        ("SELECT 'drop_chunks(' AS s", StatementClass::Read),
    ] {
        assert_eq!(ts.classify(sql), class, "timescaledb: {sql}");
    }
    // Every flavour knows TimescaleDB's functions: a `postgresql` connection may reach a
    // database with the extension (security review M1, L1).
    for d in [&pg as &dyn Dialect, &yb, &q, &ts] {
        for sql in [
            "SELECT drop_chunks('t', 1)",
            "SELECT add_retention_policy('t', INTERVAL '7 days')",
            "SELECT delete_job(1000)",
            "SELECT freeze_chunk('_timescaledb_internal._hyper_1_1_chunk')",
            "SELECT unfreeze_chunk(c) FROM show_chunks('t') c",
            "CALL distributed_exec('DROP TABLE t')",
            "SELECT add_data_node('dn1', host => 'h')",
            "SELECT create_distributed_hypertable('t', 'time')",
            "CALL timescaledb_experimental.copy_chunk('c', 'a', 'b')",
            "SELECT set_replication_factor('t', 2)",
            "SELECT _timescaledb_functions.any_function(1)",
            "SELECT \"_timescaledb_internal\".f()",
            "SELECT * FROM timescaledb_experimental.\"policies\"()",
        ] {
            assert_eq!(d.classify(sql), StatementClass::Unknown, "{sql}");
        }
        // Reading the catalogs is not a call.
        assert_eq!(
            d.classify("SELECT * FROM _timescaledb_catalog.hypertable"),
            StatementClass::Read
        );
    }
    for (sql, class) in [
        (
            "ALTER TABLE orders DROP PARTITION LIST '2025-01'",
            StatementClass::Destructive,
        ),
        (
            "ALTER TABLE orders DROP PARTITION WHERE ordered_at < '2025-06-01'",
            StatementClass::Destructive,
        ),
        (
            "ALTER TABLE orders DETACH PARTITION LIST '2025-01'",
            StatementClass::Destructive,
        ),
        ("DROP ALL TABLES", StatementClass::Destructive),
        ("DROP TABLE orders", StatementClass::Destructive),
        ("TRUNCATE TABLE orders", StatementClass::Destructive),
        (
            "UPDATE orders SET quantity = 0",
            StatementClass::Destructive,
        ),
        (
            "UPDATE orders SET quantity = 0 WHERE id = 1",
            StatementClass::Write,
        ),
        (
            "INSERT INTO orders SELECT * FROM orders",
            StatementClass::Write,
        ),
        ("RENAME TABLE orders TO orders_old", StatementClass::Unknown),
        ("VACUUM TABLE orders", StatementClass::Unknown),
        (
            "REINDEX TABLE orders COLUMN status LOCK EXCLUSIVE",
            StatementClass::Unknown,
        ),
        ("SNAPSHOT PREPARE", StatementClass::Unknown),
        ("CHECKPOINT CREATE", StatementClass::Unknown),
        ("BACKUP TABLE orders", StatementClass::Unknown),
        ("COPY orders FROM 'orders.csv'", StatementClass::Unknown),
        ("CANCEL QUERY 42", StatementClass::Unknown),
        ("REFRESH MATERIALIZED VIEW mv FULL", StatementClass::Unknown),
        (
            "DECLARE @lo := 1 SELECT * FROM orders WHERE id > @lo",
            StatementClass::Unknown,
        ),
        ("SELECT reload_config()", StatementClass::Unknown),
        (
            "SELECT * FROM orders LATEST ON ordered_at PARTITION BY status",
            StatementClass::Read,
        ),
        (
            "SELECT ordered_at, count() FROM orders SAMPLE BY 1h",
            StatementClass::Read,
        ),
        ("SELECT * FROM tables()", StatementClass::Read),
        ("SHOW TABLES", StatementClass::Read),
        ("EXPLAIN SELECT * FROM orders", StatementClass::Read),
    ] {
        assert_eq!(q.classify(sql), class, "questdb: {sql}");
        assert!(
            q.classify(sql) >= pg.classify(sql),
            "never below PostgreSQL: {sql}"
        );
    }
    // A leading SELECT never lowers the class, and every engine refuses two statements.
    for d in [&ts as &dyn Dialect, &q, &yb] {
        assert!(d.classify("SELECT 1; DROP ALL TABLES") >= StatementClass::Destructive);
        assert_eq!(d.split_statements("SELECT 1; SELECT 2").len(), 2);
    }
    assert!(q.explain("SELECT 1", ExplainFormat::Analyze).is_none());
    assert!(ts.explain("SELECT 1", ExplainFormat::Analyze).is_some());
}
