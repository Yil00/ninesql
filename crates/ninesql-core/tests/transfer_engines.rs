//! Transfer service (P5-C) against real engines: the phase 5 exit criterion as far as the core
//! goes. Exports the dev PostgreSQL `big_events` table (1 M rows of the "nine" dataset) to CSV
//! in bounded memory, and imports a 100 000-line CSV into the dev PostgreSQL, the dev MySQL and
//! a SQLite file.
//!
//! PostgreSQL and MySQL need `python3 dev/ready.py up core`; they are skipped (not failed) when
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
    eprintln!("skipped: {engine} unreachable at {addr} (python3 dev/ready.py up core)");
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

#[test]
fn postgresql_big_events_export_and_100k_import() {
    let Some(dev) = dev("postgresql") else { return };
    let tmp = tempfile::tempdir().unwrap();
    let (manager, _) = manager(
        Arc::new(ninesql_driver_pgwire::PgWireDriver::new()),
        tmp.path(),
    );
    let transfer = TransferService::new(manager.clone(), &paths(&tmp.path().join("ninesql")));
    let out = tempfile::tempdir().unwrap();
    ui(async {
        let h = open_with(
            &manager,
            tcp_connection(&dev, "postgresql"),
            Some(&dev.password),
        )
        .await;
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
            match tokio::time::timeout(Duration::from_secs(300), job.next())
                .await
                .unwrap()
            {
                Some(ExportEvent::Done(s)) => break s,
                Some(ExportEvent::Failed(e)) => panic!("{e}"),
                Some(_) => {}
                None => panic!("ended"),
            }
        };
        eprintln!(
            "postgresql big_events export: {} rows, {} bytes, {:?}, peak buffered {} bytes (batch budget {budget})",
            summary.rows,
            summary.bytes,
            started.elapsed(),
            summary.peak_buffered_bytes
        );
        assert_eq!(summary.rows, 1_000_000);
        assert!(
            summary.peak_buffered_bytes
                <= (ninesql_core::transfer::WRITER_QUEUE as u64 + 2) * budget
        );
        assert_eq!(std::fs::metadata(&target).unwrap().len(), summary.bytes);

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
                Some(ExportEvent::Failed(e)) => panic!("{e}"),
                Some(_) => {}
                None => panic!("ended"),
            }
        }
        assert_eq!(std::fs::read_to_string(&ro).unwrap(), "ro\non\n");
        let deadline = Instant::now() + Duration::from_secs(5);
        while h.is_leased() || h.transaction_state().in_transaction {
            assert!(
                Instant::now() < deadline,
                "the read-only transaction stayed open"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // 100 000-line CSV import.
        let file = write_csv(out.path());
        let table = format!("p5c_import_{}", std::process::id());
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
        let events = exec_ok(&h, &format!("SELECT label FROM {table} WHERE id = 1")).await;
        let label = events.iter().find_map(|e| match e {
            ExecEvent::Item(ResultItem::Rows(b)) => match cell(b.column(0), 0) {
                CellValue::Text(t) => Some(t.to_owned()),
                _ => None,
            },
            _ => None,
        });
        assert_eq!(label.as_deref(), Some("it's \"row\", 1\nline 2"));
        exec_ok(&h, &format!("DROP TABLE {table}")).await;
    });
}

#[test]
fn mysql_100k_import() {
    let Some(dev) = dev("mysql") else { return };
    let tmp = tempfile::tempdir().unwrap();
    let (manager, _) = manager(
        Arc::new(ninesql_driver_mysqlwire::MySqlWireDriver::new()),
        tmp.path(),
    );
    let transfer = TransferService::new(manager.clone(), &paths(&tmp.path().join("ninesql")));
    let out = tempfile::tempdir().unwrap();
    ui(async {
        let h = open_with(&manager, tcp_connection(&dev, "mysql"), Some(&dev.password)).await;
        let file = write_csv(out.path());
        let table = format!("p5c_import_{}", std::process::id());
        exec_ok(
            &h,
            &format!("CREATE TABLE {table} (id bigint PRIMARY KEY, label varchar(200), amount decimal(12,2), born date)"),
        )
        .await;
        import(
            &transfer,
            &h,
            &file,
            vec![table.clone()],
            "bigint",
            ImportTransaction::CommitEvery(25_000),
        )
        .await;
        assert_eq!(count(&h, &table).await, IMPORT_ROWS as i64);
        exec_ok(&h, &format!("DROP TABLE {table}")).await;
    });
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
