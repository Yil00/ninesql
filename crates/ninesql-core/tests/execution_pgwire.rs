//! Execution services (P3-C) against the dev PostgreSQL with the real pgwire driver: the
//! phase 3 exit criterion as far as the core goes (first rows of a 1 M-row `SELECT`, cancel
//! while it streams, `DELETE` without `WHERE` on production asks for confirmation), the error
//! position and the pending-transaction state.
//!
//! Needs the dev PostgreSQL (`python3 dev/ready.py up core`). Skipped (not failed) when it is
//! unreachable, unless `NINESQL_REQUIRE_ENGINES` lists `postgresql` (AGENTS.md).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stderr)] // Tests may panic (AGENTS.md).

mod support;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ninesql_core::driver_api::arrow_array::StringArray;
use ninesql_core::driver_api::{FetchBudget, ResultItem};
use ninesql_core::{
    Auth, Connection, ConnectionTypeId, ConnectionTypes, Endpoint, EngineId, ExecutionService,
    OpenOutcome, OpenRequest, Preferences, RunEnd, RunEvent, RunRequest, SecretSlot, SecretSource,
    TlsMode, TransactionEnd,
};
use ninesql_driver_pgwire::PgWireDriver;
use ninesql_secrets::SecretString;
use support::*;

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

fn dev() -> Option<Dev> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../dev/engines.json");
    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
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
        !required("postgresql"),
        "postgresql is required but unreachable at {addr}"
    );
    eprintln!("skipped: postgresql unreachable at {addr} (python3 dev/ready.py up core)");
    None
}

fn pg_connection(dev: &Dev, ty: ConnectionTypeId) -> Connection {
    let mut c = Connection::new(
        "Dev PostgreSQL",
        EngineId::new("postgresql").unwrap(),
        Endpoint::Tcp {
            host: dev.host.clone(),
            port: Some(dev.port),
            database: Some(dev.database.clone()),
        },
        Auth::Password {
            username: dev.user.clone(),
            secret: SecretSource::Prompt,
        },
        ty,
    );
    c.tls.mode = TlsMode::Disable;
    c
}

#[test]
fn big_select_streams_first_rows_fast_and_cancels_and_production_deletes_ask() {
    let Some(dev) = dev() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let (manager, _) = manager(Arc::new(PgWireDriver::new()), tmp.path());
    let exec = ExecutionService::new(manager.clone(), None, Preferences::default());
    let types = ConnectionTypes::default();
    let open_pg = |ty: ConnectionTypeId| {
        let request = OpenRequest::new(pg_connection(&dev, ty), &types)
            .with_secret(SecretSlot::Auth, SecretString::from(dev.password.clone()));
        let manager = manager.clone();
        async move {
            match open(&manager, request).await.unwrap() {
                OpenOutcome::Opened(h) => h,
                other => panic!("{other:?}"),
            }
        }
    };
    ui(async {
        let h = open_pg(ConnectionTypeId::dev()).await;
        let started = Instant::now();
        let mut run = exec.run(
            &h,
            RunRequest::statement(
                "SELECT g, md5(g::text) AS label FROM generate_series(1, 1000000) AS g",
            ),
        );
        let mut first_rows = None;
        let mut rows = 0usize;
        loop {
            match tokio::time::timeout(Duration::from_secs(30), run.next())
                .await
                .unwrap()
            {
                Some(RunEvent::Item {
                    item: ResultItem::Rows(batch),
                    ..
                }) => {
                    first_rows.get_or_insert_with(|| started.elapsed());
                    rows += batch.num_rows();
                    if rows >= 2_000 {
                        break;
                    }
                }
                Some(RunEvent::Done(s)) => panic!("ended before the cancel: {s:?}"),
                Some(_) => {}
                None => panic!("no Done"),
            }
        }
        let first_rows = first_rows.unwrap();
        eprintln!("time to first rows (1 M-row SELECT, debug build): {first_rows:?}");
        assert!(
            first_rows < Duration::from_secs(3),
            "first rows took {first_rows:?}"
        );
        run.cancel();
        let summary = loop {
            match tokio::time::timeout(Duration::from_secs(30), run.next())
                .await
                .unwrap()
            {
                Some(RunEvent::Done(s)) => break s,
                Some(_) => {}
                None => panic!("no Done"),
            }
        };
        assert_eq!(summary.end, RunEnd::Cancelled { index: 0 });
        // The session is usable right after (the cancel was confirmed by the server).
        let (events, summary) = collect_all(exec.run(&h, RunRequest::statement("SELECT 1"))).await;
        assert_eq!(summary, RunEnd::Completed, "{events:?}");

        // Error position (F-SQL-12): PostgreSQL reports the character offset.
        let script = "SELECT 1;\nSELECT nope_column FROM generate_series(1, 1)";
        let (events, end) = collect_all(exec.run(&h, RunRequest::script(script))).await;
        assert_eq!(end, RunEnd::Failed { index: 1 });
        let position = events
            .iter()
            .find_map(|e| match e {
                RunEvent::StatementFailed { position, .. } => *position,
                _ => None,
            })
            .expect("position");
        assert!(script[position.offset..].starts_with("nope_column"));
        assert_eq!((position.line, position.column), (2, 8));
        // Review M1: a write that returns rows, in a paged script, is never released (a
        // release is a server-side cancel, which would roll it back): it runs to its end.
        let script = "CREATE TEMP TABLE p3c_returning (i int);\n\
            INSERT INTO p3c_returning SELECT g FROM generate_series(1, 1000) AS g;\n\
            DELETE FROM p3c_returning WHERE i > 0 RETURNING i;\n\
            SELECT count(*)::text FROM p3c_returning";
        let (events, end) = collect_all(
            exec.run(
                &h,
                RunRequest::script(script)
                    .with_budget(FetchBudget::default().with_batch_rows(50))
                    .with_paging(100),
            ),
        )
        .await;
        assert_eq!(end, RunEnd::Completed, "{events:?}");
        let deleted: usize = events
            .iter()
            .map(|e| match e {
                RunEvent::Item {
                    index: 2,
                    item: ResultItem::Rows(b),
                } => b.num_rows(),
                _ => 0,
            })
            .sum();
        assert_eq!(deleted, 1000, "every RETURNING row streamed");
        assert!(!events.iter().any(|e| matches!(
            e,
            RunEvent::StatementClosed { index: 2, .. } | RunEvent::PageEnd { index: 2, .. }
        )));
        let count = events.iter().find_map(|e| match e {
            RunEvent::Item {
                index: 3,
                item: ResultItem::Rows(b),
            } if b.num_rows() > 0 => b
                .column(0)
                .as_any()
                .downcast_ref::<StringArray>()
                .map(|c| c.value(0).to_owned()),
            _ => None,
        });
        assert_eq!(
            count.as_deref(),
            Some("0"),
            "the DELETE was not rolled back"
        );
        // Review H2: after `SET standard_conforming_strings = off`, `\'` escapes the quote, so
        // the rest of the script is one SELECT of a string. Split up front (with the mode on),
        // it would have been a SELECT and a DROP TABLE.
        let (_, end) = collect_all(exec.run(
            &h,
            RunRequest::statement("CREATE TEMP TABLE p3c_canary (i int)"),
        ))
        .await;
        assert_eq!(end, RunEnd::Completed);
        let script =
            "SET standard_conforming_strings = off;\nSELECT 'a\\'; DROP TABLE p3c_canary; --' AS v";
        let mut run = exec.run(&h, RunRequest::script(script));
        let mut started = 0;
        let mut value = None;
        let summary = loop {
            match tokio::time::timeout(Duration::from_secs(30), run.next())
                .await
                .unwrap()
            {
                Some(RunEvent::StatementStarted { .. }) => started += 1,
                Some(RunEvent::Item {
                    index: 1,
                    item: ResultItem::Rows(b),
                }) if b.num_rows() > 0 => {
                    value = b
                        .column(0)
                        .as_any()
                        .downcast_ref::<StringArray>()
                        .map(|c| c.value(0).to_owned());
                }
                Some(RunEvent::Done(s)) => break s,
                Some(_) => {}
                None => panic!("no Done"),
            }
        };
        assert_eq!(summary.end, RunEnd::Completed, "{summary:?}");
        assert_eq!(
            started, 2,
            "the DROP TABLE was never a statement of its own"
        );
        assert_eq!(summary.statements, 2);
        assert_eq!(value.as_deref(), Some("a'; DROP TABLE p3c_canary; --"));
        let (events, end) =
            collect_all(exec.run(&h, RunRequest::statement("SELECT count(*) FROM p3c_canary")))
                .await;
        assert_eq!(
            end,
            RunEnd::Completed,
            "the canary table still exists: {events:?}"
        );
        h.close().await.unwrap().unwrap();

        // Production: manual commit, and a DELETE without WHERE asks first.
        let p = open_pg(ConnectionTypeId::prod()).await;
        assert!(!p.info().auto_commit);
        let script = "CREATE TEMP TABLE p3c_scratch (i int);\nINSERT INTO p3c_scratch VALUES (1), (2);\nDELETE FROM p3c_scratch";
        let (events, end) = collect_all(exec.run(&p, RunRequest::script(script))).await;
        assert_eq!(end, RunEnd::NeedsConfirmation { index: 2 }, "{events:?}");
        assert!(p.transaction_state().in_transaction);
        let token = events
            .iter()
            .find_map(|e| match e {
                RunEvent::NeedsConfirmation { request, .. } => Some(request.token),
                _ => None,
            })
            .unwrap();
        let (events, end) = collect_all(exec.run(
            &p,
            RunRequest::script(script).resume_at(2).confirmed(2, token),
        ))
        .await;
        assert_eq!(end, RunEnd::Completed);
        assert!(events.iter().any(|e| matches!(
            e,
            RunEvent::StatementFinished {
                index: 2,
                update_count: Some(2),
                ..
            }
        )));
        assert!(p.transaction_state().in_transaction);
        p.close_with(TransactionEnd::Rollback).await.unwrap();
    });
    manager.shutdown(Duration::from_secs(5));
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
