//! Integration tests against the `tidb` service of `dev/compose.yaml` (P4-C, ADR-0009).
//!
//! Skipped when TiDB is unreachable, unless `NINESQL_REQUIRE_ENGINES` lists `tidb`. Start it
//! with `python3 dev/ready.py up sql`.
//!
//! TiDB has no session read-only mode, so the read-only test switches the dev instance to
//! `tidb_super_read_only` for a moment (as `root`, whose dev password the `tidb-seed` service
//! sets): it takes the write side of [`INSTANCE`], every other test the read side, so no
//! other test of this binary runs meanwhile. The instance is switched back before and after.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stderr)]

use std::path::PathBuf;
use std::time::Duration;

use futures_util::FutureExt as _;
use ninesql_driver_api::arrow_array::cast::AsArray;
use ninesql_driver_api::arrow_array::types::{Int32Type, Int64Type};
use ninesql_driver_api::arrow_array::{Array, RecordBatch};
use ninesql_driver_api::arrow_schema::DataType;
use ninesql_driver_api::{
    Auth, ConnectParams, Dialect, Driver, DriverError, Endpoint, EngineId, ExecOptions,
    ExplainFormat, FetchBudget, IsolationLevel, Metadata, ObjectDetails, ObjectInfo, ObjectKind,
    ObjectPath, PageRequest, Param, ReadOnlyMechanism, ResultItem, RowChange, RowEdit, Secret,
    Session, Statement, StatementClass, TlsMode, conformance,
};
use ninesql_driver_mysqlwire::{MyDialect, MySqlWireDriver};
use tokio::sync::RwLock;

/// Serializes the instance-wide read-only switch against every other test.
static INSTANCE: RwLock<()> = RwLock::const_new(());

/// `root`'s dev-only password, set by the `tidb-seed` service (`dev/engines.json` notes).
const ROOT_PASSWORD: &str = "nine_dev_only_root";

#[derive(Clone)]
struct Dev {
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

fn dev() -> Option<Dev> {
    let doc = engines_json();
    let e = doc["engines"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == "tidb")
        .unwrap();
    let dev = Dev {
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
        .any(|s| s.trim() == "tidb");
    assert!(!required, "tidb is required but unreachable at {addr}");
    eprintln!("skipped: tidb unreachable at {addr} (python3 dev/ready.py up sql)");
    None
}

fn params(dev: &Dev) -> ConnectParams {
    let mut p = ConnectParams::new(EngineId::TIDB, Endpoint::tcp(&dev.host, dev.port));
    p.auth = Auth::password(&dev.user, Some(Secret::new(dev.password.clone())));
    p.database = Some(dev.database.clone());
    p.tls.mode = TlsMode::Disable;
    p.application_name = "ninesql-mysqlwire-tidb-tests".into();
    p.connect_timeout = Duration::from_secs(30);
    p
}

fn root_params(dev: &Dev) -> ConnectParams {
    let mut p = params(dev);
    p.auth = Auth::password("root", Some(Secret::new(ROOT_PASSWORD.into())));
    p
}

async fn connect(p: ConnectParams) -> Box<dyn Session> {
    match MySqlWireDriver::new().connect(p).await {
        Ok(s) => s,
        Err(e) => panic!("tidb: connect failed: {e}"),
    }
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

async fn run_stmt(
    s: &mut dyn Session,
    stmt: Statement,
    opts: ExecOptions,
) -> Result<Collected, DriverError> {
    let mut stream = s.execute(stmt, opts).await?;
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
    run_stmt(s, Statement::sql(sql), ExecOptions::default()).await
}

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
        other => format!("{other:?}"),
    })
}

/// Sets `tidb_super_read_only` on the instance and waits until a new session sees it (TiDB
/// propagates global variables asynchronously).
async fn set_instance_read_only(dev: &Dev, on: bool) {
    let mut root = connect(root_params(dev)).await;
    let v = if on { 1 } else { 0 };
    run(
        root.as_mut(),
        &format!("SET GLOBAL tidb_super_read_only = {v}"),
    )
    .await
    .unwrap();
    if !on {
        run(root.as_mut(), "SET GLOBAL tidb_restricted_read_only = 0")
            .await
            .unwrap();
    }
    root.close().await.unwrap();
    for _ in 0..100 {
        let mut s = connect(params(dev)).await;
        let c = run(s.as_mut(), "SELECT @@GLOBAL.tidb_super_read_only")
            .await
            .unwrap();
        s.close().await.unwrap();
        if scalar(&c).as_deref() == Some(&v.to_string()) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("tidb_super_read_only did not reach {v}");
}

// ---------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tidb_connects_and_other_engines_are_refused() {
    let Some(dev) = dev() else { return };
    let _shared = INSTANCE.read().await;
    let s = connect(params(&dev)).await;
    assert_eq!(s.capabilities().engine, EngineId::TIDB);
    assert!(s.server_version().unwrap().contains("TiDB"));
    s.close().await.unwrap();
    for other in [EngineId::MYSQL, EngineId::MARIADB] {
        let mut p = params(&dev);
        p.engine = other.clone();
        match MySqlWireDriver::new().connect(p).await {
            Err(DriverError::Connect(m)) => {
                assert!(m.contains("TiDB") && m.contains("choose"), "{m}");
                assert!(!m.contains(&dev.password));
            }
            other_outcome => panic!("TiDB accepted as {other}: {:?}", other_outcome.map(|_| ())),
        }
    }
    // A TiDB connection to the MySQL service is refused too.
    let doc = engines_json();
    let mysql = doc["engines"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == "mysql")
        .unwrap()
        .clone();
    let port = u16::try_from(mysql["port"].as_u64().unwrap()).unwrap();
    let reachable = format!("127.0.0.1:{port}")
        .parse()
        .ok()
        .and_then(|a| std::net::TcpStream::connect_timeout(&a, Duration::from_secs(1)).ok())
        .is_some();
    if reachable {
        let mut p = ConnectParams::new(EngineId::TIDB, Endpoint::tcp("127.0.0.1", port));
        p.auth = Auth::password(
            mysql["user"].as_str().unwrap(),
            Some(Secret::new(mysql["password"].as_str().unwrap().into())),
        );
        p.tls.mode = TlsMode::Disable;
        assert!(matches!(
            MySqlWireDriver::new().connect(p).await,
            Err(DriverError::Connect(_))
        ));
    }
}

/// Dev-only password of the accounts created by the read-only test.
const M1_PASSWORD: &str = "ninesql_m1_dev_only";

/// Accounts for the read-only test (phase 4 review M1), created as `root` while the instance is
/// writable. `ninesql_m1_repl` holds `RESTRICTED_REPLICA_WRITER_ADMIN`, with which TiDB lets it
/// write on a read-only instance; `ninesql_m1_rolew` holds it through its default role;
/// `ninesql_m1_dormant` has that role granted but not active; `ninesql_m1_super` holds `SUPER`.
const M1_ACCOUNTS: &[&str] = &[
    "DROP USER IF EXISTS ninesql_m1_repl, ninesql_m1_rolew, ninesql_m1_dormant, ninesql_m1_super",
    "DROP ROLE IF EXISTS ninesql_m1_writer",
    "CREATE ROLE ninesql_m1_writer",
    "GRANT RESTRICTED_REPLICA_WRITER_ADMIN ON *.* TO ninesql_m1_writer",
    "CREATE USER ninesql_m1_repl IDENTIFIED BY 'ninesql_m1_dev_only'",
    "CREATE USER ninesql_m1_rolew IDENTIFIED BY 'ninesql_m1_dev_only'",
    "CREATE USER ninesql_m1_dormant IDENTIFIED BY 'ninesql_m1_dev_only'",
    "CREATE USER ninesql_m1_super IDENTIFIED BY 'ninesql_m1_dev_only'",
    "GRANT SELECT, INSERT, DELETE ON nine.* TO ninesql_m1_repl, ninesql_m1_rolew, \
     ninesql_m1_dormant, ninesql_m1_super",
    "GRANT RESTRICTED_REPLICA_WRITER_ADMIN ON *.* TO ninesql_m1_repl",
    "GRANT SUPER ON *.* TO ninesql_m1_super",
    "GRANT ninesql_m1_writer TO ninesql_m1_rolew, ninesql_m1_dormant",
    "SET DEFAULT ROLE ninesql_m1_writer TO ninesql_m1_rolew",
];

const M1_DROP: &[&str] = &[
    "DROP USER IF EXISTS ninesql_m1_repl, ninesql_m1_rolew, ninesql_m1_dormant, ninesql_m1_super",
    "DROP ROLE IF EXISTS ninesql_m1_writer",
];

fn m1_params(dev: &Dev, user: &str, read_only: bool) -> ConnectParams {
    let mut p = params(dev);
    p.auth = Auth::password(user, Some(Secret::new(M1_PASSWORD.into())));
    p.read_only = read_only;
    p
}

async fn run_all(p: ConnectParams, sqls: &[&str]) {
    let mut s = connect(p).await;
    for sql in sqls {
        if let Err(e) = run(s.as_mut(), sql).await {
            panic!("{sql:?}: {e}");
        }
    }
    s.close().await.unwrap();
}

/// A read-only session must be refused for `user`, naming why.
async fn assert_read_only_refused(p: ConnectParams, user: &str, reason: &str) {
    match MySqlWireDriver::new().connect(p).await {
        Err(DriverError::ReadOnly(m)) => {
            assert!(m.contains(reason), "{user}: {m}");
            assert!(
                !m.contains(M1_PASSWORD) && !m.contains(ROOT_PASSWORD),
                "{m}"
            );
        }
        other => panic!(
            "{user}: read-only session accepted for an account TiDB lets write: {:?}",
            other.map(|_| ())
        ),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_only_needs_a_read_only_instance_and_is_enforced_engine_side() {
    let Some(dev) = dev() else { return };
    let _exclusive = INSTANCE.write().await;
    set_instance_read_only(&dev, false).await;
    let driver = MySqlWireDriver::new();
    assert_eq!(
        driver
            .capabilities(&EngineId::TIDB, None)
            .unwrap()
            .read_only,
        ReadOnlyMechanism::ServerSetting { liftable: true }
    );
    // A writable instance: a read-only session is refused, never faked.
    let mut p = params(&dev);
    p.read_only = true;
    match driver.connect(p).await {
        Err(DriverError::ReadOnly(m)) => assert!(m.contains("tidb_super_read_only"), "{m}"),
        other => panic!(
            "read-only session on a writable TiDB: {:?}",
            other.map(|_| ())
        ),
    }
    // Fixtures and accounts, while the instance is writable.
    run_all(params(&dev), BROWSE_FIXTURE).await;
    run_all(
        params(&dev),
        &[
            "DROP TABLE IF EXISTS ninesql_m1_written",
            "CREATE TABLE ninesql_m1_written (a INT)",
        ],
    )
    .await;
    run_all(root_params(&dev), M1_ACCOUNTS).await;
    let body = async {
        set_instance_read_only(&dev, true).await;
        // Client-side refusals in a read-only session.
        let writes = conformance::writes_for(&EngineId::TIDB);
        conformance::assert_read_only_refuses_writes(&driver, params(&dev), &writes).await;
        let mut p = params(&dev);
        p.read_only = true;
        let mut ro = connect(p).await;
        assert!(ro.is_read_only());
        let c = run(ro.as_mut(), "SELECT count(*) FROM orders")
            .await
            .unwrap();
        assert_eq!(scalar(&c).as_deref(), Some("10000"));
        // The "nine" dataset browses fully on the read-only session (phase 4 exit criterion).
        assert_nine_browses_read_only(ro.as_ref(), &dev).await;
        // Engine-side: a session not marked read-only cannot write either (error 1836).
        let mut rw = connect(params(&dev)).await;
        for sql in [
            "INSERT INTO nine_meta VALUES ('p4c', 'x')",
            "UPDATE nine_meta SET value = 'x' WHERE `key` = 'seed_version'",
            "CREATE TABLE ninesql_p4c_ro (a INT)",
        ] {
            assert!(
                matches!(run(rw.as_mut(), sql).await, Err(DriverError::ReadOnly(_))),
                "{sql}"
            );
        }
        rw.close().await.unwrap();

        // Phase 4 review M1: accounts the instance's mode does not stop are refused.
        // `RESTRICTED_REPLICA_WRITER_ADMIN` really writes on the read-only instance...
        let mut repl = connect(m1_params(&dev, "ninesql_m1_repl", false)).await;
        let c = run(repl.as_mut(), "INSERT INTO ninesql_m1_written VALUES (1)")
            .await
            .unwrap();
        assert_eq!(c.counts, [Some(1)]);
        run(repl.as_mut(), "DELETE FROM ninesql_m1_written")
            .await
            .unwrap();
        repl.close().await.unwrap();
        // ...so its read-only session is refused, directly or through an active role.
        for user in ["ninesql_m1_repl", "ninesql_m1_rolew"] {
            assert_read_only_refused(
                m1_params(&dev, user, true),
                user,
                "RESTRICTED_REPLICA_WRITER_ADMIN",
            )
            .await;
        }
        // `SUPER` and `ALL PRIVILEGES ON *.*` (root) are refused too.
        assert_read_only_refused(
            m1_params(&dev, "ninesql_m1_super", true),
            "ninesql_m1_super",
            "SUPER",
        )
        .await;
        let mut p = root_params(&dev);
        p.read_only = true;
        assert_read_only_refused(p, "root", "ALL PRIVILEGES").await;
        // A role granted but not active: accepted, and it cannot be enabled.
        let mut dormant = connect(m1_params(&dev, "ninesql_m1_dormant", true)).await;
        assert!(dormant.is_read_only());
        for sql in [
            "SET ROLE ALL",
            "SET ROLE ninesql_m1_writer",
            "SET DEFAULT ROLE ALL TO ninesql_m1_dormant",
            "GRANT RESTRICTED_REPLICA_WRITER_ADMIN ON *.* TO ninesql_m1_dormant",
            "INSERT INTO ninesql_m1_written VALUES (1)",
        ] {
            assert!(
                matches!(
                    run(dormant.as_mut(), sql).await,
                    Err(DriverError::ReadOnly(_))
                ),
                "{sql}"
            );
        }
        let c = run(dormant.as_mut(), "SELECT CURRENT_ROLE()")
            .await
            .unwrap();
        assert_eq!(scalar(&c).as_deref(), Some("NONE"));
        dormant.close().await.unwrap();
        // A manual-commit read-only session: the write probe leaves no transaction open.
        let mut p = params(&dev);
        p.read_only = true;
        p.auto_commit = false;
        let mut manual = connect(p).await;
        assert!(manual.is_read_only());
        assert!(!manual.in_transaction());
        run(manual.as_mut(), "SELECT 1").await.unwrap();
        manual.close().await.unwrap();

        // A privileged user lifts the instance's mode: the read-only session notices it after
        // a statement run once the change has propagated, and closes.
        let mut root = connect(root_params(&dev)).await;
        run(root.as_mut(), "SET GLOBAL tidb_super_read_only = 0")
            .await
            .unwrap();
        root.close().await.unwrap();
        let mut lifted = false;
        for _ in 0..100 {
            match run(ro.as_mut(), "SELECT 1").await {
                Ok(_) => tokio::time::sleep(Duration::from_millis(100)).await,
                Err(DriverError::ReadOnly(_)) => {
                    lifted = true;
                    break;
                }
                Err(e) => panic!("unexpected error: {e:?}"),
            }
        }
        assert!(lifted, "the lifted instance mode went unnoticed");
        assert!(matches!(
            run(ro.as_mut(), "SELECT 1").await,
            Err(DriverError::Closed(_))
        ));
    };
    let outcome = std::panic::AssertUnwindSafe(body).catch_unwind().await;
    set_instance_read_only(&dev, false).await;
    run_all(root_params(&dev), M1_DROP).await;
    run_all(
        params(&dev),
        &[
            "DROP SEQUENCE IF EXISTS ninesql_p4c_seq",
            "DROP TABLE IF EXISTS ninesql_p4c_part",
            "DROP TABLE IF EXISTS ninesql_m1_written",
        ],
    )
    .await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn budgets_cancel_and_timeouts() {
    let Some(dev) = dev() else { return };
    let _shared = INSTANCE.read().await;
    let driver = MySqlWireDriver::new();
    let budget = FetchBudget::default()
        .with_batch_rows(5_000)
        .with_max_batch_bytes(512 * 1024)
        .with_max_value_bytes(40)
        .with_max_row_bytes(160)
        .with_max_bytes(1024 * 1024 * 1024);
    let report = conformance::assert_batches_within_budget(
        &driver,
        params(&dev),
        "SELECT id, event_time, kind, value, payload, REPEAT(kind, 30) AS wide FROM big_events",
        budget,
    )
    .await;
    assert_eq!(report.rows, 1_000_000);
    assert!(report.truncated_values);
    conformance::assert_multiple_statements_refused(&driver, params(&dev)).await;
    conformance::assert_reserved_properties_rejected(&driver, &params(&dev)).await;
    let mut s = connect(params(&dev)).await;
    let started = std::time::Instant::now();
    let outcome = run_stmt(
        s.as_mut(),
        Statement::sql("SELECT SLEEP(60)"),
        ExecOptions::default().with_timeout(Some(Duration::from_secs(2))),
    )
    .await;
    assert!(
        matches!(outcome, Err(DriverError::Timeout(_))),
        "{:?}",
        outcome.map(|_| ())
    );
    assert!(started.elapsed() < Duration::from_secs(20));
    assert_eq!(
        run_when_idle(s.as_mut(), "SELECT 1").await.unwrap().rows(),
        1
    );
    // A dropped stream mid-result leaves the session usable.
    {
        let mut stream = s
            .execute(
                Statement::sql("SELECT * FROM big_events"),
                ExecOptions::default().with_budget(FetchBudget::default().with_batch_rows(100)),
            )
            .await
            .unwrap();
        while let Some(item) = stream.next().await {
            if let ResultItem::Rows(_) = item.unwrap() {
                break;
            }
        }
    }
    let c = run_when_idle(s.as_mut(), "SELECT count(*) FROM products")
        .await
        .unwrap();
    assert_eq!(scalar(&c).as_deref(), Some("100"));
    s.close().await.unwrap();
    conformance::assert_stale_cancel_ignored(
        &driver,
        params(&dev),
        "SELECT SLEEP(0.2)",
        "SELECT count(*) FROM products",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transactions_edits_explain_and_parameters() {
    let Some(dev) = dev() else { return };
    let _shared = INSTANCE.read().await;
    let mut s = connect(params(&dev)).await;
    let caps = s.capabilities().clone();
    for level in [
        IsolationLevel::ReadCommitted,
        IsolationLevel::RepeatableRead,
    ] {
        s.set_isolation_level(level).await.unwrap();
    }
    for level in [
        IsolationLevel::Serializable,
        IsolationLevel::ReadUncommitted,
    ] {
        assert!(matches!(
            s.set_isolation_level(level).await,
            Err(DriverError::Unsupported(_))
        ));
    }
    run(s.as_mut(), "DROP TABLE IF EXISTS ninesql_p4c_edit")
        .await
        .unwrap();
    run(
        s.as_mut(),
        "CREATE TABLE ninesql_p4c_edit (id INT PRIMARY KEY, name VARCHAR(100), note TEXT)",
    )
    .await
    .unwrap();
    // Manual commit and savepoints.
    s.set_auto_commit(false).await.unwrap();
    run(
        s.as_mut(),
        "INSERT INTO ninesql_p4c_edit VALUES (100, 'a', NULL)",
    )
    .await
    .unwrap();
    s.savepoint("sp1").await.unwrap();
    run(
        s.as_mut(),
        "INSERT INTO ninesql_p4c_edit VALUES (101, 'b', NULL)",
    )
    .await
    .unwrap();
    s.rollback_to_savepoint("sp1").await.unwrap();
    assert!(s.in_transaction());
    s.commit().await.unwrap();
    s.set_auto_commit(true).await.unwrap();
    let c = run(s.as_mut(), "SELECT count(*) FROM ninesql_p4c_edit")
        .await
        .unwrap();
    assert_eq!(scalar(&c).as_deref(), Some("1"));
    // Grid edits rendered by the dialect, with hostile text.
    let dialect = s.dialect();
    let table = vec![dev.database.clone(), "ninesql_p4c_edit".to_owned()];
    let tricky = "it's \\' \"x\" `y` ; -- /* z".to_owned();
    for (change, count) in [
        (
            RowChange::Insert {
                values: vec![
                    ("id".into(), Param::Int(1)),
                    ("name".into(), Param::Text(tricky.clone())),
                    ("note".into(), Param::Null),
                ],
            },
            1,
        ),
        (
            RowChange::Update {
                key: vec![("id".into(), Param::Int(1))],
                values: vec![("note".into(), Param::Text("n".into()))],
            },
            1,
        ),
    ] {
        let stmt = dialect
            .render_edit(&RowEdit {
                table: table.clone(),
                change,
            })
            .unwrap();
        let c = run_stmt(s.as_mut(), stmt, ExecOptions::default())
            .await
            .unwrap();
        assert_eq!(c.counts, vec![Some(count)]);
    }
    let mut check = Statement::sql("SELECT note FROM ninesql_p4c_edit WHERE name = ?");
    check.params = vec![Param::Text(tricky.clone())];
    let c = run_stmt(s.as_mut(), check, ExecOptions::default())
        .await
        .unwrap();
    assert_eq!(scalar(&c).as_deref(), Some("n"));
    let stmt = dialect
        .render_edit(&RowEdit {
            table,
            change: RowChange::Delete {
                key: vec![("id".into(), Param::Int(1))],
            },
        })
        .unwrap();
    let c = run_stmt(s.as_mut(), stmt, ExecOptions::default())
        .await
        .unwrap();
    assert_eq!(c.counts, vec![Some(1)]);
    run(s.as_mut(), "DROP TABLE ninesql_p4c_edit")
        .await
        .unwrap();
    // Explain in the declared formats.
    for format in [
        ExplainFormat::Text,
        ExplainFormat::Json,
        ExplainFormat::Analyze,
    ] {
        let sql = dialect.explain("SELECT * FROM orders WHERE id = 1", format);
        assert_eq!(sql.is_some(), caps.explain.contains(&format), "{format:?}");
        if let Some(sql) = sql {
            assert!(
                run(s.as_mut(), &sql).await.unwrap().rows() > 0,
                "{format:?}"
            );
        }
    }
    // The "nine" all_types table decodes (row 3 is all NULLs).
    let c = run(s.as_mut(), "SELECT * FROM all_types ORDER BY id")
        .await
        .unwrap();
    assert_eq!(c.rows(), 3);
    s.close().await.unwrap();
}

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
    }
    out
}

fn names(items: &[ObjectInfo]) -> Vec<String> {
    items
        .iter()
        .map(|i| i.path.leaf().unwrap().name.clone())
        .collect()
}

/// Objects the read-only browse expects besides the "nine" dataset, created while the
/// instance is writable.
const BROWSE_FIXTURE: &[&str] = &[
    "DROP SEQUENCE IF EXISTS ninesql_p4c_seq",
    "CREATE SEQUENCE ninesql_p4c_seq START WITH 10 INCREMENT BY 5",
    "DROP TABLE IF EXISTS ninesql_p4c_part",
    "CREATE TABLE ninesql_p4c_part (id INT, y INT) PARTITION BY RANGE (y) \
     (PARTITION p0 VALUES LESS THAN (2000), PARTITION p1 VALUES LESS THAN MAXVALUE)",
];

/// Phase 4 exit criterion: the "nine" dataset browses fully on a read-only session (the
/// instance read-only), every node and every declared DDL.
async fn assert_nine_browses_read_only(ro: &dyn Session, dev: &Dev) {
    assert!(ro.is_read_only());
    let m = ro.metadata();
    assert_eq!(
        m.child_kinds(Some(ObjectKind::Schema)),
        vec![ObjectKind::Table, ObjectKind::View, ObjectKind::Sequence]
    );
    let schemas = names(&all(m.as_ref(), &ObjectPath::root(), ObjectKind::Schema).await);
    assert!(schemas.contains(&dev.database), "{schemas:?}");
    let schema = ObjectPath::root().child(ObjectKind::Schema, &dev.database);
    let tables = names(&all(m.as_ref(), &schema, ObjectKind::Table).await);
    for t in [
        "all_types",
        "big_events",
        "customers",
        "nine_meta",
        "orders",
        "products",
    ] {
        assert!(tables.contains(&t.to_owned()), "{tables:?}");
    }
    let views = names(&all(m.as_ref(), &schema, ObjectKind::View).await);
    assert_eq!(views, ["customer_order_totals"]);
    let seqs = names(&all(m.as_ref(), &schema, ObjectKind::Sequence).await);
    assert!(seqs.contains(&"ninesql_p4c_seq".to_owned()), "{seqs:?}");
    let seq = schema.child(ObjectKind::Sequence, "ninesql_p4c_seq");
    assert!(m.ddl(&seq).await.unwrap().contains("CREATE SEQUENCE"));
    let orders = schema.child(ObjectKind::Table, "orders");
    assert_eq!(all(m.as_ref(), &orders, ObjectKind::Column).await.len(), 6);
    let fks = all(m.as_ref(), &orders, ObjectKind::ForeignKey).await;
    assert_eq!(fks.len(), 2);
    match &fks[0].details {
        ObjectDetails::ForeignKey(f) => assert!(!f.columns.is_empty()),
        other => panic!("{other:?}"),
    }
    let indexes = names(&all(m.as_ref(), &orders, ObjectKind::Index).await);
    assert!(indexes.contains(&"PRIMARY".to_owned()), "{indexes:?}");
    assert!(
        indexes.contains(&"orders_customer_idx".to_owned()),
        "{indexes:?}"
    );
    let constraints = all(m.as_ref(), &orders, ObjectKind::Constraint).await;
    assert!(!constraints.is_empty());
    let part = schema.child(ObjectKind::Table, "ninesql_p4c_part");
    assert_eq!(
        names(&all(m.as_ref(), &part, ObjectKind::Partition).await),
        ["p0", "p1"]
    );
    // Every declared kind of every object lists, and every declared DDL generates (the
    // schema's included).
    let r = conformance::assert_read_only_browse(ro, schema, &|_| false).await;
    assert!(r.visited > 40, "{r:?}");
    assert!(r.count(ObjectKind::Partition) >= 2, "{r:?}");
    assert!(r.count(ObjectKind::Sequence) >= 1, "{r:?}");
    assert!(r.ddl >= 9, "{r:?}");
}

#[test]
fn tidb_dialect_overrides() {
    let d = MyDialect::tidb();
    let my = MyDialect::mysql();
    // TiDB runs `/*T! … */`: never below Unknown, whatever the comment holds.
    for sql in [
        "CREATE TABLE t (a BIGINT PRIMARY KEY /*T![clustered_index] CLUSTERED */)",
        "SELECT 1 /*T! , sleep(1) */",
        "UPDATE t SET a = 1 WHERE id = 1 /*t! */",
    ] {
        assert_eq!(d.classify(sql), StatementClass::Unknown, "{sql}");
        assert!(my.classify(sql) <= StatementClass::Unknown);
    }
    // Inside a string it is text.
    assert_eq!(d.classify("SELECT '/*T! x */'"), StatementClass::Read);
    assert_eq!(
        d.classify("SELECT 1 /* T! not executable */"),
        StatementClass::Read
    );
    assert_eq!(
        d.explain("SELECT 1", ExplainFormat::Json).as_deref(),
        Some("EXPLAIN FORMAT = 'tidb_json' SELECT 1")
    );
    assert_eq!(
        d.explain("SELECT 1", ExplainFormat::Analyze).as_deref(),
        Some("EXPLAIN ANALYZE SELECT 1")
    );
    assert!(d.classify("SET GLOBAL tidb_super_read_only = 0") >= StatementClass::ChangesAccessMode);
    assert!(
        d.classify("SET GLOBAL tidb_restricted_read_only = OFF")
            >= StatementClass::ChangesAccessMode
    );
    // Role and privilege changes are refused in a read-only session (phase 4 review M1: a
    // role may hold RESTRICTED_REPLICA_WRITER_ADMIN, which TiDB's read-only mode exempts).
    for sql in [
        "SET ROLE ALL",
        "SET ROLE DEFAULT",
        "SET ROLE NONE",
        "SET ROLE 'r'@'%'",
        "set role `r`",
        "SET DEFAULT ROLE ALL TO u",
        "SET PASSWORD = 'x'",
        "GRANT RESTRICTED_REPLICA_WRITER_ADMIN ON *.* TO u",
        "GRANT r TO u",
        "REVOKE SELECT ON *.* FROM u",
        "CREATE ROLE r",
        "DROP ROLE r",
        "CREATE USER u",
        "ALTER USER u ACCOUNT UNLOCK",
    ] {
        assert!(d.classify(sql).is_write(), "{sql}: {:?}", d.classify(sql));
    }
    // Sequences and user-level locks: side effects a `SELECT` can carry (security review L4),
    // on every engine of the family.
    for d in [&d as &dyn Dialect, &my, &MyDialect::mariadb()] {
        for sql in [
            "SELECT NEXTVAL(s)",
            "SELECT nextval(nine.s), 1",
            "SELECT SETVAL(s, 100)",
            "SELECT LASTVAL(s)",
            "SELECT NEXT VALUE FOR s",
            "SELECT NEXTVAL FOR s",
            "SELECT PREVIOUS VALUE FOR s",
            "SELECT GET_LOCK('l', 10)",
            "SELECT RELEASE_LOCK('l')",
            "SELECT release_all_locks()",
            "INSERT INTO t VALUES (NEXT VALUE FOR s)",
        ] {
            assert_eq!(d.classify(sql), StatementClass::Unknown, "{sql}");
        }
        for sql in [
            "SELECT 'GET_LOCK(' AS s",
            "SELECT `get_lock` FROM t",
            "SELECT IS_FREE_LOCK('l')",
            "SELECT next_value FROM t",
        ] {
            assert_eq!(d.classify(sql), StatementClass::Read, "{sql}");
        }
    }
}

/// Navigator reads in manual-commit mode end the transaction they started (TiDB starts one on
/// any statement with `autocommit = 0`), never one the user started; another session's DDL on
/// a browsed table does not wait for the browsing session (TiDB's metadata lock).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn navigator_reads_leave_no_transaction_and_keep_the_users() {
    let Some(dev) = dev() else { return };
    let _shared = INSTANCE.read().await;
    let mut admin = connect(params(&dev)).await;
    for sql in [
        "DROP TABLE IF EXISTS ninesql_tidb_meta_txn",
        "CREATE TABLE ninesql_tidb_meta_txn (id INT PRIMARY KEY, v INT, KEY v_idx (v))",
    ] {
        run(admin.as_mut(), sql).await.unwrap();
    }
    let sch = ObjectPath::root().child(ObjectKind::Schema, &dev.database);
    let table = sch.child(ObjectKind::Table, "ninesql_tidb_meta_txn");
    async fn browse(s: &dyn Session, sch: &ObjectPath, table: &ObjectPath, expected: bool) {
        let m = s.metadata();
        m.list(
            &ObjectPath::root(),
            ObjectKind::Schema,
            PageRequest::first(),
        )
        .await
        .unwrap();
        assert_eq!(s.in_transaction(), expected, "after listing schemas");
        m.list(sch, ObjectKind::Table, PageRequest::first())
            .await
            .unwrap();
        assert_eq!(s.in_transaction(), expected, "after listing tables");
        for kind in [
            ObjectKind::Column,
            ObjectKind::Index,
            ObjectKind::Constraint,
        ] {
            m.list(table, kind, PageRequest::first()).await.unwrap();
            assert_eq!(s.in_transaction(), expected, "after listing {kind:?}");
        }
        m.ddl(table).await.unwrap();
        assert_eq!(s.in_transaction(), expected, "after the DDL");
    }
    let mut s = connect(params(&dev)).await;
    s.set_auto_commit(false).await.unwrap();
    assert!(!s.in_transaction());
    browse(s.as_ref(), &sch, &table, false).await;
    let ddl = async {
        run(
            admin.as_mut(),
            "ALTER TABLE ninesql_tidb_meta_txn ADD COLUMN extra INT",
        )
        .await
        .unwrap();
        run(
            admin.as_mut(),
            "ALTER TABLE ninesql_tidb_meta_txn DROP COLUMN extra",
        )
        .await
        .unwrap();
    };
    tokio::time::timeout(Duration::from_secs(20), ddl)
        .await
        .expect("DDL waited for the browsing session");

    // The user's own transaction (manual commit) stays open with its uncommitted row.
    run(
        s.as_mut(),
        "INSERT INTO ninesql_tidb_meta_txn VALUES (1, 10)",
    )
    .await
    .unwrap();
    assert!(s.in_transaction());
    browse(s.as_ref(), &sch, &table, true).await;
    let count = "SELECT CAST(COUNT(*) AS CHAR) FROM ninesql_tidb_meta_txn";
    assert_eq!(
        scalar(&run(s.as_mut(), count).await.unwrap()).as_deref(),
        Some("1")
    );
    assert_eq!(
        scalar(&run(admin.as_mut(), count).await.unwrap()).as_deref(),
        Some("0")
    );
    s.rollback().await.unwrap();
    assert!(!s.in_transaction());
    s.set_auto_commit(true).await.unwrap();
    assert_eq!(
        scalar(&run(s.as_mut(), count).await.unwrap()).as_deref(),
        Some("0")
    );
    s.close().await.unwrap();
    run(admin.as_mut(), "DROP TABLE ninesql_tidb_meta_txn")
        .await
        .unwrap();
    admin.close().await.unwrap();
}
