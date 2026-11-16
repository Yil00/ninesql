//! Execution services against the dev MySQL and MariaDB with the real mysqlwire driver: a
//! paged read left paused past the idle limit is closed by the run before the server's
//! `net_write_timeout` ends the session, and an open transaction in manual commit survives it
//! (phase 3 review M1).
//!
//! Needs the dev MySQL / MariaDB (`python3 dev/ready.py up core`). Skipped (not failed) when an
//! engine is unreachable, unless `NINESQL_REQUIRE_ENGINES` lists it (AGENTS.md).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stderr)] // Tests may panic (AGENTS.md).

mod support;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ninesql_core::driver_api::ResultItem;
use ninesql_core::driver_api::arrow_array::{Array, Int64Array, StringArray};
use ninesql_core::{
    Auth, Connection, ConnectionTypeId, ConnectionTypes, Endpoint, EngineId, ExecutionService,
    OpenOutcome, OpenRequest, Preferences, RunEnd, RunEvent, RunRequest, SecretSlot, SecretSource,
    TlsMode, TransactionEnd,
};
use ninesql_driver_mysqlwire::MySqlWireDriver;
use ninesql_secrets::SecretString;
use support::*;

struct Dev {
    engine: &'static str,
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

fn dev(engine: &'static str) -> Option<Dev> {
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
        engine,
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

fn connection(dev: &Dev) -> Connection {
    let mut c = Connection::new(
        "Dev MySQL",
        EngineId::new(dev.engine).unwrap(),
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
    // Manual commit: the case where losing the session would roll the transaction back.
    c.auto_commit = Some(false);
    c
}

async fn collect_all(mut run: ninesql_core::Run) -> (Vec<RunEvent>, RunEnd) {
    let mut events = Vec::new();
    loop {
        match tokio::time::timeout(Duration::from_secs(30), run.next())
            .await
            .unwrap()
        {
            Some(RunEvent::Done(s)) => return (events, s.end),
            Some(e) => events.push(e),
            None => panic!("no Done"),
        }
    }
}

fn first_value(events: &[RunEvent]) -> Option<String> {
    events.iter().find_map(|e| match e {
        RunEvent::Item {
            item: ResultItem::Rows(b),
            ..
        } if b.num_rows() > 0 => {
            let col = b.column(0);
            if let Some(s) = col.as_any().downcast_ref::<StringArray>() {
                Some(s.value(0).to_owned())
            } else {
                col.as_any()
                    .downcast_ref::<Int64Array>()
                    .filter(|a| !a.is_null(0))
                    .map(|a| a.value(0).to_string())
            }
        }
        _ => None,
    })
}

fn idle_close_keeps_the_transaction(engine: &'static str) {
    let Some(dev) = dev(engine) else { return };
    let tmp = tempfile::tempdir().unwrap();
    let (manager, _) = manager(Arc::new(MySqlWireDriver::new()), tmp.path());
    let prefs = Preferences::default();
    let idle = Duration::from_secs(1);
    let budget = prefs.execution.budget().with_batch_rows(100);
    let exec = ExecutionService::new(manager.clone(), None, prefs);
    // The test hook: the preference is clamped to at least 30 s.
    exec.set_paused_result_idle_for_tests(Some(idle));
    let types = ConnectionTypes::default();
    ui(async {
        let request = OpenRequest::new(connection(&dev), &types)
            .with_secret(SecretSlot::Auth, SecretString::from(dev.password.clone()));
        let h = match open(&manager, request).await.unwrap() {
            OpenOutcome::Opened(h) => h,
            other => panic!("{other:?}"),
        };
        assert!(!h.info().auto_commit);
        let script = "CREATE TEMPORARY TABLE p3r_idle (i INT) ENGINE=InnoDB;\nINSERT INTO p3r_idle VALUES (1), (2), (3)";
        let (events, end) = collect_all(exec.run(&h, RunRequest::script(script))).await;
        assert_eq!(end, RunEnd::Completed, "{events:?}");
        assert!(h.transaction_state().in_transaction);

        // Enough rows that the server is still sending when the grid pauses.
        let big = "WITH RECURSIVE d (i) AS (SELECT 0 UNION ALL SELECT i + 1 FROM d WHERE i < 999) \
            SELECT a.i * 1000 + b.i AS n, REPEAT('x', 200) AS pad FROM d AS a CROSS JOIN d AS b";
        let started = std::time::Instant::now();
        let mut run = exec.run(
            &h,
            RunRequest::statement(big)
                .with_budget(budget)
                .with_paging(100),
        );
        let mut events = Vec::new();
        let summary = loop {
            match tokio::time::timeout(Duration::from_secs(60), run.next())
                .await
                .unwrap()
            {
                Some(RunEvent::Done(s)) => break s,
                Some(e) => {
                    if matches!(
                        e,
                        RunEvent::PageEnd { .. }
                            | RunEvent::ResultClosedIdle { .. }
                            | RunEvent::StatementClosed { .. }
                    ) {
                        eprintln!("{engine}: {e:?} at {:?}", started.elapsed());
                    }
                    events.push(e);
                }
                None => panic!("no Done"),
            }
        };
        assert_eq!(summary.end, RunEnd::Completed, "{events:?}");
        // Released promptly: the session is not held long past the idle limit.
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "{:?}",
            started.elapsed()
        );
        let closed = events
            .iter()
            .position(|e| {
                matches!(
                    e,
                    RunEvent::ResultClosedIdle {
                        index: 0,
                        rows: 100,
                        ..
                    }
                )
            })
            .unwrap_or_else(|| panic!("no ResultClosedIdle: {events:?}"));
        assert!(
            matches!(
                events.get(closed + 1),
                Some(RunEvent::StatementClosed {
                    index: 0,
                    rows: 100
                })
            ),
            "{events:?}"
        );
        assert!(!h.is_closed());
        assert!(
            h.transaction_state().in_transaction,
            "the open transaction is not ended by the idle close"
        );
        let (events, end) = collect_all(exec.run(
            &h,
            RunRequest::statement("SELECT CAST(COUNT(*) AS CHAR) FROM p3r_idle"),
        ))
        .await;
        assert_eq!(end, RunEnd::Completed, "{events:?}");
        assert_eq!(
            first_value(&events).as_deref(),
            Some("3"),
            "the transaction still holds the write"
        );
        assert!(h.transaction_state().in_transaction);
        h.close_with(TransactionEnd::Rollback).await.unwrap();
    });
    manager.shutdown(Duration::from_secs(5));
}

#[test]
fn mysql_idle_paused_read_is_closed_without_touching_the_transaction() {
    idle_close_keeps_the_transaction("mysql");
}

#[test]
fn mariadb_idle_paused_read_is_closed_without_touching_the_transaction() {
    idle_close_keeps_the_transaction("mariadb");
}
