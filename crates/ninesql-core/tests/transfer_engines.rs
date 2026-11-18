//! Transfer service (P5-C) against real engines: the phase 5 exit criterion as far as the core
//! goes. Exports the dev PostgreSQL `big_events` table (1 M rows of the "nine" dataset) to CSV
//! in bounded memory, and imports a 100 000-line CSV into the dev PostgreSQL, the dev MySQL, the
//! dev MariaDB and a SQLite file. The same export and import run on the dev YugabyteDB.
//!
//! Also (P5-E): imports into a MyISAM table on MySQL and MariaDB ("All or nothing" refused, rows
//! written reported as kept), and the export's read-only transaction on MySQL (used: a writing
//! function is refused by the engine) and on TiDB (none: a warning, refused on strict types).
//!
//! PostgreSQL, MySQL and MariaDB need `python3 dev/ready.py up core`, TiDB and YugabyteDB
//! `python3 dev/ready.py up sql --engines tidb,yugabytedb`; they are skipped (not failed) when
//! unreachable, unless `NINESQL_REQUIRE_ENGINES` lists them (AGENTS.md). SQLite always runs.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stderr)] // Tests may panic (AGENTS.md).

mod support;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ninesql_core::driver_api::{Driver, ResultItem};
use ninesql_core::transfer::value::{CellValue, cell};
use ninesql_core::transfer::{ColumnMapping, ExportFormat, ExportOptions, TargetColumn};
use ninesql_core::{
    Auth, Connection, ConnectionTypeId, ConnectionTypes, Endpoint, EngineId, ExecEvent,
    ExportEvent, ExportRequest, ImportEnd, ImportEvent, ImportRequest, ImportTransaction, Job,
    OpenOutcome, OpenRequest, SecretSlot, SecretSource, SessionHandle, TlsMode, TransferService,
};
use ninesql_secrets::SecretString;
use ninesql_storage::{Paths, XdgDirs};
use support::*;

const IMPORT_ROWS: usize = 100_000;

struct Dev {
    host: String,
    port: u16,
    user: String,
    password: String,
    database: String,
}

fn required(engine: &str) -> bool {
    std::env::var("NINESQL_REQUIRE_ENGINES")
        .unwrap_or_default()
        .split([',', ' '])
        .any(|e| e.trim() == engine || e.trim() == "all")
}

fn dev(engine: &str) -> Option<Dev> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../dev/engines.json");
    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let e = doc["engines"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == engine)
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
    assert!(
        !required(engine),
        "{engine} is required but unreachable at {addr}"
    );
    eprintln!("skipped: {engine} unreachable at {addr} (python3 dev/ready.py up core / sql)");
    None
}

fn tcp_connection(dev: &Dev, engine: &str) -> Connection {
    let mut c = Connection::new(
        "Dev",
        EngineId::new(engine).unwrap(),
        Endpoint::Tcp {
            host: dev.host.clone(),
            port: Some(dev.port),
            database: Some(dev.database.clone()),
        },
        Auth::Password {
            username: dev.user.clone(),
            secret: SecretSource::Prompt,
        },
        ConnectionTypeId::dev(),
    );
    c.tls.mode = TlsMode::Disable;
    c
}

fn paths(root: &Path) -> Paths {
    Paths::new(&XdgDirs {
        config_home: root.join("config"),
        data_home: root.join("data"),
        state_home: root.join("state"),
    })
}

async fn open_with(
    manager: &ninesql_core::SessionManager,
    c: Connection,
    password: Option<&str>,
) -> SessionHandle {
    let mut request = OpenRequest::new(c, &ConnectionTypes::default());
    if let Some(p) = password {
        request = request.with_secret(SecretSlot::Auth, SecretString::from(p.to_owned()));
    }
    match open(manager, request).await.unwrap() {
        OpenOutcome::Opened(h) => h,
        other => panic!("{other:?}"),
    }
}

async fn exec_ok(h: &SessionHandle, sql: &str) -> Vec<ExecEvent> {
    let events = run(h, sql).await;
    assert!(
        matches!(terminal(&events), ExecEvent::Finished { .. }),
        "{sql}: {:?}",
        terminal(&events)
    );
    events
}

async fn count(h: &SessionHandle, table: &str) -> i64 {
    let events = exec_ok(h, &format!("SELECT count(*) FROM {table}")).await;
    for e in &events {
        if let ExecEvent::Item(ResultItem::Rows(b)) = e {
            return match cell(b.column(0), 0) {
                CellValue::Int(i) => i,
                CellValue::UInt(u) => i64::try_from(u).unwrap(),
                CellValue::Decimal(d) => d.parse().unwrap(),
                CellValue::Text(t) => t.parse().unwrap(),
                other => panic!("{other:?}"),
            };
        }
    }
    panic!("no row")
}

fn write_csv(dir: &Path) -> PathBuf {
    let path = dir.join("import-100k.csv");
    let mut f = std::io::BufWriter::new(std::fs::File::create(&path).unwrap());
    writeln!(f, "id,label,amount,born").unwrap();
    for i in 0..IMPORT_ROWS {
        // Quotes, delimiters and line breaks inside values; NULLs every 10th row.
        if i % 10 == 0 {
            writeln!(f, "{i},,,").unwrap();
        } else {
            writeln!(
                f,
                "{i},\"it's \"\"row\"\", {i}\nline 2\",{}.{:02},2000-{:02}-{:02}",
                i / 7,
                i % 100,
                i % 12 + 1,
                i % 28 + 1
            )
            .unwrap();
        }
    }
    f.flush().unwrap();
    path
}

fn targets(int_type: &str) -> Vec<TargetColumn> {
    vec![
        TargetColumn::new("id", int_type).with_nullable(false),
        TargetColumn::new("label", "varchar(200)"),
        TargetColumn::new("amount", "decimal(12,2)"),
        TargetColumn::new("born", "date"),
    ]
}

async fn import(
    transfer: &TransferService,
    h: &SessionHandle,
    file: &Path,
    table: Vec<String>,
    int_type: &str,
    transaction: ImportTransaction,
) {
    let mapping = ColumnMapping::by_position(4, &targets(int_type));
    let started = Instant::now();
    let mut job: Job<ImportEvent> = transfer.import_csv(
        h,
        ImportRequest::new(file, table, targets(int_type), mapping).with_transaction(transaction),
    );
    let summary = loop {
        match tokio::time::timeout(Duration::from_secs(300), job.next())
            .await
            .unwrap()
        {
            Some(ImportEvent::Done(s)) => break s,
            Some(ImportEvent::Failed(e)) => panic!("refused: {e}"),
            Some(ImportEvent::NeedsConfirmation(_)) => panic!("confirmation on dev"),
            Some(_) => {}
            None => panic!("ended"),
        }
    };
    eprintln!(
        "{}: imported {} rows in {} statements, {} commits, {:?}",
        h.info().engine.as_str(),
        summary.rows_inserted,
        summary.statements,
        summary.commits,
        started.elapsed()
    );
    assert_eq!(summary.end, ImportEnd::Completed, "{:?}", summary.error);
    assert_eq!(summary.rows_inserted, IMPORT_ROWS as u64);
    assert_eq!(summary.rows_committed, IMPORT_ROWS as u64);
    assert!(!h.transaction_state().in_transaction);
}

/// The label the CSV gives row `id` (`id` not a multiple of 10).
fn expected_label(id: usize) -> String {
    format!("it's \"row\", {id}\nline 2")
}

async fn label_of(h: &SessionHandle, table: &str, id: usize) -> Option<String> {
    let events = exec_ok(h, &format!("SELECT label FROM {table} WHERE id = {id}")).await;
    events.iter().find_map(|e| match e {
        ExecEvent::Item(ResultItem::Rows(b)) => match cell(b.column(0), 0) {
            CellValue::Text(t) => Some(t.to_owned()),
            CellValue::Bytes(t) => Some(String::from_utf8(t.to_vec()).unwrap()),
            _ => None,
        },
        _ => None,
    })
}

/// PostgreSQL-wire engines: `big_events` (1 M rows of "nine") exported to CSV in bounded
/// memory inside an engine-side read-only transaction, then a 100 000-line CSV imported "All
/// or nothing".
fn pgwire_big_events_export_and_100k_import(engine: &str) {
    let Some(dev) = dev(engine) else { return };
    let tmp = tempfile::tempdir().unwrap();
    let (manager, _) = manager(
        Arc::new(ninesql_driver_pgwire::PgWireDriver::new()),
        tmp.path(),
    );
    let transfer = TransferService::new(manager.clone(), &paths(&tmp.path().join("ninesql")));
    let out = tempfile::tempdir().unwrap();
    ui(async {
        let h = open_with(&manager, tcp_connection(&dev, engine), Some(&dev.password)).await;
        // 1 M rows of big_events to CSV.
        let target = out.path().join("big_events.csv");
        let request = ExportRequest::table(
            vec!["public".into(), "big_events".into()],
            &target,
            ExportOptions::new(ExportFormat::Csv),
        );
        let budget = request.budget.effective_max_batch_bytes();
        let started = Instant::now();
        let mut job = transfer.export(&h, request);
        let summary = loop {
            match tokio::time::timeout(Duration::from_secs(600), job.next())
                .await
                .unwrap()
            {
                Some(ExportEvent::Done(s)) => break s,
                Some(ExportEvent::Failed(e)) => panic!("{engine}: {e}"),
                Some(_) => {}
                None => panic!("ended"),
            }
        };
        eprintln!(
            "{engine} big_events export: {} rows, {} bytes, {:?}, peak buffered {} bytes (batch budget {budget})",
            summary.rows,
            summary.bytes,
            started.elapsed(),
            summary.peak_buffered_bytes
        );
        assert_eq!(summary.rows, 1_000_000, "{engine}");
        assert!(
            summary.warnings.is_empty(),
            "{engine}: {:?}",
            summary.warnings
        );
        assert!(
            summary.peak_buffered_bytes
                <= (ninesql_core::transfer::WRITER_QUEUE as u64 + 2) * budget
        );
        assert_eq!(std::fs::metadata(&target).unwrap().len(), summary.bytes);
        let _ = std::fs::remove_file(&target);

        // The export ran in an engine-side read-only transaction (review L7), ended after it.
        let ro = out.path().join("ro.csv");
        let mut job = transfer.export(
            &h,
            ExportRequest::statement(
                "SELECT current_setting('transaction_read_only') AS ro",
                &ro,
                ExportOptions::new(ExportFormat::Csv),
            ),
        );
        loop {
            match job.next().await {
                Some(ExportEvent::Done(_)) => break,
                Some(ExportEvent::Failed(e)) => panic!("{engine}: {e}"),
                Some(_) => {}
                None => panic!("ended"),
            }
        }
        assert_eq!(
            std::fs::read_to_string(&ro).unwrap(),
            "ro\non\n",
            "{engine}"
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while h.is_leased() || h.transaction_state().in_transaction {
            assert!(
                Instant::now() < deadline,
                "{engine}: the read-only transaction stayed open"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // 100 000-line CSV import.
        let file = write_csv(out.path());
        let table = format!("p5c_import_{}", std::process::id());
        let _ = run(&h, &format!("DROP TABLE IF EXISTS {table}")).await;
        exec_ok(
            &h,
            &format!("CREATE TABLE {table} (id bigint PRIMARY KEY, label varchar(200), amount numeric(12,2), born date)"),
        )
        .await;
        import(
            &transfer,
            &h,
            &file,
            vec!["public".into(), table.clone()],
            "bigint",
            ImportTransaction::AllOrNothing,
        )
        .await;
        assert_eq!(count(&h, &table).await, IMPORT_ROWS as i64);
        assert_eq!(
            label_of(&h, &table, 1).await.as_deref(),
            Some(expected_label(1).as_str()),
            "{engine}"
        );
        exec_ok(&h, &format!("DROP TABLE {table}")).await;
    });
}

#[test]
fn postgresql_big_events_export_and_100k_import() {
    pgwire_big_events_export_and_100k_import("postgresql");
}

#[test]
fn yugabytedb_big_events_export_and_100k_import() {
    pgwire_big_events_export_and_100k_import("yugabytedb");
}

/// MySQL family: the 100 000-line CSV imported once per transaction mode, "commit every N
/// rows" then "All or nothing", into InnoDB tables.
fn mysql_family_100k_import(engine: &str) {
    let Some(dev) = dev(engine) else { return };
    let tmp = tempfile::tempdir().unwrap();
    let (manager, _) = manager(
        Arc::new(ninesql_driver_mysqlwire::MySqlWireDriver::new()),
        tmp.path(),
    );
    let transfer = TransferService::new(manager.clone(), &paths(&tmp.path().join("ninesql")));
    let out = tempfile::tempdir().unwrap();
    ui(async {
        let h = open_with(&manager, tcp_connection(&dev, engine), Some(&dev.password)).await;
        let file = write_csv(out.path());
        for (suffix, transaction) in [
            ("every", ImportTransaction::CommitEvery(25_000)),
            ("all", ImportTransaction::AllOrNothing),
        ] {
            let table = format!("p5c_import_{suffix}_{}", std::process::id());
            exec_ok(&h, &format!("DROP TABLE IF EXISTS {table}")).await;
            exec_ok(
                &h,
                &format!("CREATE TABLE {table} (id bigint PRIMARY KEY, label varchar(200), amount decimal(12,2), born date) ENGINE=InnoDB"),
            )
            .await;
            // Schema-qualified, so that the table's storage is read: "All or nothing" is
            // refused on the MySQL family when it is unknown (P5-E M1).
            import(
                &transfer,
                &h,
                &file,
                vec![dev.database.clone(), table.clone()],
                "bigint",
                transaction,
            )
            .await;
            assert_eq!(count(&h, &table).await, IMPORT_ROWS as i64, "{engine}");
            assert_eq!(
                label_of(&h, &table, 1).await.as_deref(),
                Some(expected_label(1).as_str()),
                "{engine}"
            );
            assert_eq!(label_of(&h, &table, 10).await, None, "{engine}: NULL label");
            exec_ok(&h, &format!("DROP TABLE {table}")).await;
        }
    });
}

#[test]
fn mysql_100k_import() {
    mysql_family_100k_import("mysql");
}

#[test]
fn mariadb_100k_import() {
    mysql_family_100k_import("mariadb");
}

#[test]
fn sqlite_100k_import() {
    let tmp = tempfile::tempdir().unwrap();
    let driver = ninesql_driver_sqlite::SqliteDriver::new();
    let engine = driver.engines()[0].clone();
    let (manager, _) = manager(Arc::new(driver), tmp.path());
    let transfer = TransferService::new(manager.clone(), &paths(&tmp.path().join("ninesql")));
    let out = tempfile::tempdir().unwrap();
    let db = out.path().join("import.db");
    // The driver opens existing files only.
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute_batch(
            "CREATE TABLE people (id INTEGER PRIMARY KEY, label TEXT, amount NUMERIC, born DATE)",
        )
        .unwrap();
    ui(async {
        let mut c = Connection::new(
            "SQLite",
            EngineId::new(engine.as_str()).unwrap(),
            Endpoint::File { path: db.clone() },
            Auth::None,
            ConnectionTypeId::dev(),
        );
        c.tls.mode = TlsMode::Disable;
        let h = open_with(&manager, c, None).await;
        let file = write_csv(out.path());
        import(
            &transfer,
            &h,
            &file,
            vec!["people".into()],
            "integer",
            ImportTransaction::AllOrNothing,
        )
        .await;
        assert_eq!(count(&h, "people").await, IMPORT_ROWS as i64);
    });
}

// ---------------------------------------------------------------------------------------------
// P5-E M1: non-transactional tables.

enum Outcome {
    Confirm(ninesql_core::ImportConfirmation),
    Refused(ninesql_core::TransferJobError),
    Done(ninesql_core::ImportSummary),
}

async fn import_outcome(mut job: Job<ImportEvent>) -> Outcome {
    loop {
        match tokio::time::timeout(Duration::from_secs(120), job.next())
            .await
            .unwrap()
        {
            Some(ImportEvent::Done(s)) => return Outcome::Done(s),
            Some(ImportEvent::Failed(e)) => return Outcome::Refused(e),
            Some(ImportEvent::NeedsConfirmation(c)) => return Outcome::Confirm(c),
            Some(_) => {}
            None => panic!("ended"),
        }
    }
}

/// 10 000 rows whose id 5 000 appears again at line 5 051 (a duplicate key the engine refuses).
fn write_duplicate_csv(dir: &Path) -> PathBuf {
    let path = dir.join("duplicate.csv");
    let mut f = std::io::BufWriter::new(std::fs::File::create(&path).unwrap());
    writeln!(f, "id,label").unwrap();
    for i in 0..10_000 {
        let id = if i == 5_050 { 5_000 } else { i };
        writeln!(f, "{id},row {i}").unwrap();
    }
    f.flush().unwrap();
    path
}

fn small_targets() -> Vec<TargetColumn> {
    vec![
        TargetColumn::new("id", "bigint").with_nullable(false),
        TargetColumn::new("label", "varchar(200)"),
    ]
}

async fn myisam_import(engine: &str, driver: Arc<dyn Driver>) {
    use ninesql_core::TransferJobError;
    let Some(dev) = dev(engine) else { return };
    let tmp = tempfile::tempdir().unwrap();
    let (manager, _) = manager(driver, tmp.path());
    let transfer = TransferService::new(manager.clone(), &paths(&tmp.path().join("ninesql")));
    let out = tempfile::tempdir().unwrap();
    let h = open_with(&manager, tcp_connection(&dev, engine), Some(&dev.password)).await;
    let myisam = format!("p5e_myisam_{}", std::process::id());
    let innodb = format!("p5e_innodb_{}", std::process::id());
    for (t, e) in [(&myisam, "MyISAM"), (&innodb, "InnoDB")] {
        exec_ok(&h, &format!("DROP TABLE IF EXISTS {t}")).await;
        exec_ok(
            &h,
            &format!("CREATE TABLE {t} (id bigint PRIMARY KEY, label varchar(200)) ENGINE={e}"),
        )
        .await;
    }
    let file = write_duplicate_csv(out.path());
    let mapping = ColumnMapping::by_position(2, &small_targets());
    let request = |t: &str| {
        ImportRequest::new(
            &file,
            vec![dev.database.clone(), t.to_owned()],
            small_targets(),
            mapping.clone(),
        )
        .with_rows_per_insert(100)
    };

    // The target as the UI reads it, found from the name parts.
    let target = transfer
        .import_target(&h, vec![dev.database.clone(), myisam.clone()], None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(target.table_transactional(), Some(false), "{engine}");
    assert!(!target.all_or_nothing_allowed());
    let target = transfer
        .import_target(&h, vec![dev.database.clone(), innodb.clone()], None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(target.table_transactional(), Some(true), "{engine}");
    assert!(target.all_or_nothing_allowed());
    // A single-part name is not found: unknown storage on the MySQL family.
    let target = transfer
        .import_target(&h, vec![myisam.clone()], None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(target.table_transactional(), None);
    assert!(matches!(
        target.all_or_nothing_refusal(),
        Some(TransferJobError::TableStorageUnknown)
    ));

    // "All or nothing" into MyISAM is refused before anything is sent.
    match import_outcome(transfer.import_csv(&h, request(&myisam))).await {
        Outcome::Refused(TransferJobError::NonTransactionalTable) => {}
        _ => panic!("{engine}: all or nothing into MyISAM must be refused"),
    }
    assert_eq!(count(&h, &myisam).await, 0);

    // "Commit every N rows" runs, fails at the duplicate, and reports the rows as kept.
    let s = match import_outcome(transfer.import_csv(
        &h,
        request(&myisam).with_transaction(ImportTransaction::CommitEvery(1_000)),
    ))
    .await
    {
        Outcome::Done(s) => s,
        _ => panic!("{engine}: not run"),
    };
    assert_eq!(s.end, ImportEnd::Failed, "{engine}");
    assert!(!s.transactional, "{engine}: MyISAM cannot roll back");
    assert_eq!(s.target.table_transactional(), Some(false));
    assert_eq!(s.rows_inserted, 5_000, "{engine}");
    assert_eq!(s.rows_committed, s.rows_inserted, "{engine}");
    let kept = count(&h, &myisam).await;
    // The failed multi-row INSERT may have written its rows before the duplicate too.
    assert!(kept >= 5_000, "{engine}: {kept} rows kept");
    assert!(!h.transaction_state().in_transaction);

    // InnoDB: "All or nothing" rolls everything back, and says so.
    let s = match import_outcome(transfer.import_csv(&h, request(&innodb))).await {
        Outcome::Done(s) => s,
        _ => panic!("{engine}: not run"),
    };
    assert_eq!(s.end, ImportEnd::Failed);
    assert!(s.transactional);
    assert_eq!(s.rows_committed, 0);
    assert_eq!(count(&h, &innodb).await, 0);

    // Production: the confirmation says rows cannot be rolled back and the import commits
    // them by itself on the manual-commit session.
    let mut c = tcp_connection(&dev, engine);
    c.connection_type = ConnectionTypeId::prod();
    let prod = open_with(&manager, c, Some(&dev.password)).await;
    match import_outcome(transfer.import_csv(
        &prod,
        request(&myisam).with_transaction(ImportTransaction::CommitEvery(1_000)),
    ))
    .await
    {
        Outcome::Confirm(c) => {
            assert!(c.non_transactional());
            assert_eq!(c.transaction(), ImportTransaction::CommitEvery(1_000));
            assert!(c.session_manual_commit());
        }
        _ => panic!("{engine}: production asks first"),
    }
    match import_outcome(transfer.import_csv(&prod, request(&innodb))).await {
        Outcome::Confirm(c) => {
            assert!(!c.non_transactional());
            assert_eq!(c.transaction(), ImportTransaction::AllOrNothing);
        }
        _ => panic!("{engine}: production asks first"),
    }
    drop(prod);
    for t in [&myisam, &innodb] {
        exec_ok(&h, &format!("DROP TABLE {t}")).await;
    }
}

#[test]
fn myisam_import_mysql() {
    ui(myisam_import(
        "mysql",
        Arc::new(ninesql_driver_mysqlwire::MySqlWireDriver::new()),
    ));
}

#[test]
fn myisam_import_mariadb() {
    ui(myisam_import(
        "mariadb",
        Arc::new(ninesql_driver_mysqlwire::MySqlWireDriver::new()),
    ));
}

// ---------------------------------------------------------------------------------------------
// P5-E M3: the export's read-only transaction on MySQL and TiDB.

async fn export_outcome(
    transfer: &TransferService,
    h: &SessionHandle,
    request: ExportRequest,
) -> Result<ninesql_core::ExportSummary, ninesql_core::TransferJobError> {
    let mut job = transfer.export(h, request);
    let result = loop {
        match tokio::time::timeout(Duration::from_secs(60), job.next())
            .await
            .unwrap()
        {
            Some(ExportEvent::Done(s)) => break Ok(s),
            Some(ExportEvent::Failed(e)) => break Err(e),
            Some(_) => {}
            None => panic!("ended"),
        }
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    while h.is_leased() || h.transaction_state().in_transaction {
        assert!(Instant::now() < deadline, "the export left its transaction");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    result
}

#[test]
fn export_read_only_transaction_mysql_and_tidb() {
    use ninesql_core::{ExportWarning, TransferJobError};
    let tmp = tempfile::tempdir().unwrap();
    let (manager, _) = manager(
        Arc::new(ninesql_driver_mysqlwire::MySqlWireDriver::new()),
        tmp.path(),
    );
    let transfer = TransferService::new(manager.clone(), &paths(&tmp.path().join("ninesql")));
    let out = tempfile::tempdir().unwrap();
    let csv = || ExportOptions::new(ExportFormat::Csv);
    ui(async {
        if let Some(dev) = dev("mysql") {
            let h = open_with(&manager, tcp_connection(&dev, "mysql"), Some(&dev.password)).await;
            // A function that writes, which the classifier reads as a read: the engine refuses
            // it inside the export's read-only transaction (error 1792).
            let probe = format!("p5e_probe_{}", std::process::id());
            let func = format!("p5e_side_effect_{}", std::process::id());
            exec_ok(&h, &format!("DROP TABLE IF EXISTS {probe}")).await;
            exec_ok(&h, &format!("CREATE TABLE {probe} (id INT)")).await;
            exec_ok(&h, &format!("DROP FUNCTION IF EXISTS {func}")).await;
            exec_ok(
                &h,
                &format!(
                    "CREATE FUNCTION {func}() RETURNS INT READS SQL DATA \
                     BEGIN INSERT INTO {probe} VALUES (1); RETURN 1; END"
                ),
            )
            .await;
            let side_effect = format!("SELECT {func}() AS x");
            assert_eq!(
                h.dialect().classify(&side_effect),
                ninesql_core::driver_api::StatementClass::Read
            );
            let e = export_outcome(
                &transfer,
                &h,
                ExportRequest::statement(&side_effect, out.path().join("m1.csv"), csv()),
            )
            .await
            .unwrap_err();
            assert!(e.to_string().contains("READ ONLY"), "{e}");
            assert_eq!(count(&h, &probe).await, 0);
            // A plain export works, without warning, also on production.
            let mut c = tcp_connection(&dev, "mysql");
            c.connection_type = ConnectionTypeId::prod();
            let prod = open_with(&manager, c, Some(&dev.password)).await;
            for session in [&h, &prod] {
                let s = export_outcome(
                    &transfer,
                    session,
                    ExportRequest::statement(
                        "SELECT 1 AS x",
                        out.path().join(format!("m-{}.csv", session.id())),
                        csv(),
                    ),
                )
                .await
                .unwrap();
                assert!(s.warnings.is_empty(), "{:?}", s.warnings);
            }
            assert!(
                export_outcome(
                    &transfer,
                    &prod,
                    ExportRequest::statement(&side_effect, out.path().join("m2.csv"), csv()),
                )
                .await
                .is_err()
            );
            assert_eq!(count(&h, &probe).await, 0);
            exec_ok(&h, &format!("DROP FUNCTION {func}")).await;
            exec_ok(&h, &format!("DROP TABLE {probe}")).await;
        }

        if let Some(dev) = dev("tidb") {
            // No read-only transaction is sent (TiDB would refuse it with error 1235, or
            // accept it as a no-op): a warning on dev, a refusal on strict types.
            let h = open_with(&manager, tcp_connection(&dev, "tidb"), Some(&dev.password)).await;
            let s = export_outcome(
                &transfer,
                &h,
                ExportRequest::statement("SELECT 1 AS x", out.path().join("t1.csv"), csv()),
            )
            .await
            .unwrap();
            assert_eq!(s.warnings, vec![ExportWarning::NoReadOnlyTransaction]);
            assert_eq!(
                std::fs::read_to_string(out.path().join("t1.csv")).unwrap(),
                "x\n1\n"
            );
            // Also with `tidb_enable_noop_functions` on, where the no-op would be accepted.
            exec_ok(&h, "SET SESSION tidb_enable_noop_functions = ON").await;
            let s = export_outcome(
                &transfer,
                &h,
                ExportRequest::statement("SELECT 1 AS x", out.path().join("t2.csv"), csv()),
            )
            .await
            .unwrap();
            assert_eq!(s.warnings, vec![ExportWarning::NoReadOnlyTransaction]);

            let mut c = tcp_connection(&dev, "tidb");
            c.connection_type = ConnectionTypeId::prod();
            let prod = open_with(&manager, c, Some(&dev.password)).await;
            assert!(matches!(
                export_outcome(
                    &transfer,
                    &prod,
                    ExportRequest::statement("SELECT 1 AS x", out.path().join("t3.csv"), csv()),
                )
                .await,
                Err(TransferJobError::NoReadOnlyTransaction)
            ));
            assert!(!out.path().join("t3.csv").exists());
            // A custom type that confirms every write is strict too.
            let mut types = ConnectionTypes::default();
            let mut confirm = ninesql_core::ConnectionType::development();
            confirm.id = ninesql_core::ConnectionTypeId::new("p5e-confirm").unwrap();
            confirm.name = "Confirm".into();
            confirm.confirm_every_write = true;
            types.upsert(confirm.clone());
            let mut c = tcp_connection(&dev, "tidb");
            c.connection_type = confirm.id.clone();
            let mut request = OpenRequest::new(c, &types);
            request =
                request.with_secret(SecretSlot::Auth, SecretString::from(dev.password.clone()));
            let OpenOutcome::Opened(strict) = open(&manager, request).await.unwrap() else {
                panic!("not opened");
            };
            assert!(matches!(
                export_outcome(
                    &transfer,
                    &strict,
                    ExportRequest::statement("SELECT 1 AS x", out.path().join("t4.csv"), csv()),
                )
                .await,
                Err(TransferJobError::NoReadOnlyTransaction)
            ));
        }
    });
}
