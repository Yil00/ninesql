//! Execution services (P3-C) against the mock driver: scripts, selections, cancel, timeouts,
//! production confirmations, error positions, transaction tracking, keep-alive, reconnect and
//! the query history (I1).
#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use ninesql_core::driver_api::mock::MockDriver;
use ninesql_core::driver_api::{
    self as api, CancelHandle, Capabilities, ConnectParams, Dialect, Driver, DriverError,
    DriverResult, ExecOptions, FetchBudget, Metadata, Param, ResultItem, ResultStream, Secret,
    Session, Statement,
};
use ninesql_core::{
    CloseReason, ConfirmationPreferences, ConnectionType, ConnectionTypeId, ConnectionTypes,
    CoreError, CoreEvent, ExecutionService, FetchMode, History, OnError, OpenOutcome, OpenRequest,
    PreferenceChange, Preferences, ReconnectEvent, ReconnectPolicy, RunEnd, RunEvent, RunRequest,
    RunSummary, SessionManager, TimeoutChoice, TransactionEnd,
};
use ninesql_storage::HistoryDb;
use ninesql_storage::history::{EntryStatus, HistoryQuery};
use support::*;

async fn collect_run(mut run: ninesql_core::Run) -> (Vec<RunEvent>, RunSummary) {
    let mut events = Vec::new();
    loop {
        match tokio::time::timeout(Duration::from_secs(20), run.next()).await {
            Ok(Some(RunEvent::Done(summary))) => return (events, summary),
            Ok(Some(e)) => events.push(e),
            Ok(None) => panic!("run ended without Done: {events:?}"),
            Err(_) => panic!("run timed out: {events:?}"),
        }
    }
}

fn rows(events: &[RunEvent], index: usize) -> usize {
    events
        .iter()
        .map(|e| match e {
            RunEvent::Item {
                index: i,
                item: ResultItem::Rows(batch),
            } if *i == index => batch.num_rows(),
            _ => 0,
        })
        .sum()
}

fn service(manager: &SessionManager, history: Option<History>) -> ExecutionService {
    ExecutionService::new(manager.clone(), history, Preferences::default())
}

#[test]
fn scripts_and_selections_run_statement_by_statement() {
    let tmp = tempfile::tempdir().unwrap();
    let (manager, _) = manager(Arc::new(mock_driver()), tmp.path());
    let types = ConnectionTypes::default();
    let exec = service(&manager, None);
    ui(async {
        let h = open_ok(&manager, mock_connection(ConnectionTypeId::dev()), &types).await;
        let script = "SELECT * FROM numbers(3);\n-- comment\nINSERT INTO t VALUES (1);\nSELECT * FROM notices(2)";
        let (events, summary) = collect_run(exec.run(&h, RunRequest::script(script))).await;
        assert_eq!(summary.statements, 3);
        assert_eq!(summary.executed, 3);
        assert_eq!(summary.succeeded, 3);
        assert_eq!(summary.end, RunEnd::Completed);
        assert_eq!(rows(&events, 0), 3);
        let spans: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                RunEvent::StatementStarted { index, span, .. } => Some((*index, span.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(spans.len(), 3);
        assert_eq!(&script[spans[1].1.clone()], "INSERT INTO t VALUES (1)");
        assert!(events.iter().any(|e| matches!(
            e,
            RunEvent::Item {
                index: 1,
                item: ResultItem::UpdateCount(Some(1))
            }
        )));
        assert!(events.iter().any(|e| matches!(
            e,
            RunEvent::StatementFinished {
                index: 1,
                update_count: Some(1),
                ..
            }
        )));
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(
                    e,
                    RunEvent::Item {
                        index: 2,
                        item: ResultItem::Notice(_)
                    }
                ))
                .count(),
            2
        );

        // A selection: spans point into the editor buffer.
        let buffer = "garbage before;SELECT * FROM numbers(1)";
        let (events, summary) =
            collect_run(exec.run(&h, RunRequest::selection(&buffer[15..], 15))).await;
        assert_eq!(summary.executed, 1);
        let span = events
            .iter()
            .find_map(|e| match e {
                RunEvent::StatementStarted { span, .. } => Some(span.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(&buffer[span], "SELECT * FROM numbers(1)");

        // "Execute statement" refuses several statements before sending anything.
        let (events, summary) =
            collect_run(exec.run(&h, RunRequest::statement("SELECT 1 AS a; SELECT 1 AS b"))).await;
        assert_eq!(summary.end, RunEnd::Refused);
        assert_eq!(summary.executed, 0);
        assert!(matches!(
            events[0],
            RunEvent::StatementFailed {
                error: CoreError::InvalidRequest(_),
                ..
            }
        ));
        // Parameters need a single statement.
        let (_, summary) = collect_run(exec.run(
            &h,
            RunRequest::script("SELECT 1 AS a; SELECT 1 AS b").with_params(vec![Param::Int(1)]),
        ))
        .await;
        assert_eq!(summary.end, RunEnd::Refused);
        // An empty script completes with nothing run.
        let (_, summary) = collect_run(exec.run(&h, RunRequest::script("  -- nothing\n"))).await;
        assert_eq!((summary.statements, summary.end), (0, RunEnd::Completed));

        // The editor helpers.
        let engine = mock_engine();
        let spans = exec.split(&engine, "SELECT 1;\nSELECT 2").unwrap();
        assert_eq!(spans.len(), 2);
        let at = exec
            .statement_at(&engine, "SELECT 1;\nSELECT 2", 12)
            .unwrap()
            .unwrap();
        assert_eq!(at, spans[1]);
    });
    manager.shutdown(Duration::from_secs(5));
}

#[test]
fn errors_carry_their_position_and_on_error_decides() {
    let tmp = tempfile::tempdir().unwrap();
    let (manager, _) = manager(Arc::new(mock_driver()), tmp.path());
    let types = ConnectionTypes::default();
    let exec = service(&manager, None);
    ui(async {
        let h = open_ok(&manager, mock_connection(ConnectionTypeId::dev()), &types).await;
        // The mock answers a statement it does not understand with a syntax error at char 1.
        let script = "SELECT * FROM numbers(1);\n  FROBNICATE things;\nSELECT * FROM numbers(2)";
        let (events, summary) = collect_run(exec.run(&h, RunRequest::script(script))).await;
        assert_eq!(summary.end, RunEnd::Failed { index: 1 });
        assert_eq!((summary.executed, summary.failed), (2, 1));
        let position = events
            .iter()
            .find_map(|e| match e {
                RunEvent::StatementFailed {
                    index: 1, position, ..
                } => Some(position.unwrap()),
                _ => None,
            })
            .unwrap();
        assert_eq!(&script[position.offset..position.offset + 10], "FROBNICATE");
        assert_eq!((position.line, position.column), (2, 3));

        let (events, summary) = collect_run(exec.run(
            &h,
            RunRequest::script(script).with_on_error(OnError::Continue),
        ))
        .await;
        assert_eq!(summary.end, RunEnd::Completed);
        assert_eq!(
            (summary.executed, summary.succeeded, summary.failed),
            (3, 2, 1)
        );
        assert_eq!(rows(&events, 2), 2);
    });
    manager.shutdown(Duration::from_secs(5));
}

#[test]
fn cancel_stops_the_running_statement_and_the_script() {
    let tmp = tempfile::tempdir().unwrap();
    let mock = mock_driver();
    let stats = mock.stats();
    let (manager, _) = manager(Arc::new(mock), tmp.path());
    let types = ConnectionTypes::default();
    let exec = service(&manager, None);
    ui(async {
        let h = open_ok(&manager, mock_connection(ConnectionTypeId::dev()), &types).await;
        let mut run = exec.run(
            &h,
            RunRequest::script(
                "SELECT * FROM slow_numbers(1000000, 20);\nSELECT * FROM numbers(5)",
            ),
        );
        // Wait for the first rows (F-SQL-05: cancel while rows stream).
        loop {
            match tokio::time::timeout(Duration::from_secs(10), run.next())
                .await
                .unwrap()
            {
                Some(RunEvent::Item {
                    item: ResultItem::Rows(_),
                    ..
                }) => break,
                Some(_) => {}
                None => panic!("ended early"),
            }
        }
        run.cancel();
        let mut statement_cancelled = false;
        let summary = loop {
            match tokio::time::timeout(Duration::from_secs(10), run.next())
                .await
                .unwrap()
            {
                Some(RunEvent::StatementCancelled { index: 0, .. }) => statement_cancelled = true,
                Some(RunEvent::StatementStarted { index: 1, .. }) => panic!("next statement ran"),
                Some(RunEvent::Done(s)) => break s,
                Some(_) => {}
                None => panic!("no Done"),
            }
        };
        assert!(statement_cancelled);
        assert_eq!(summary.end, RunEnd::Cancelled { index: 0 });
        assert!(stats.server_cancels() >= 1, "server-side cancel sent");
        assert!(stats.rows_generated() < 1_000_000);
        // The session goes on.
        let (_, summary) =
            collect_run(exec.run(&h, RunRequest::statement("SELECT * FROM numbers(2)"))).await;
        assert_eq!(summary.end, RunEnd::Completed);

        // Dropping the run cancels too.
        let generated = stats.rows_generated();
        drop(exec.run(
            &h,
            RunRequest::statement("SELECT * FROM slow_numbers(1000000, 20)"),
        ));
        let (_, summary) =
            collect_run(exec.run(&h, RunRequest::statement("SELECT * FROM numbers(1)"))).await;
        assert_eq!(summary.end, RunEnd::Completed);
        assert!(stats.rows_generated() - generated < 100_000);
    });
    manager.shutdown(Duration::from_secs(5));
}

#[test]
fn timeouts_per_request_and_per_connection() {
    let tmp = tempfile::tempdir().unwrap();
    let (manager, _) = manager(Arc::new(mock_driver()), tmp.path());
    let types = ConnectionTypes::default();
    let exec = service(&manager, None);
    ui(async {
        let c = mock_connection(ConnectionTypeId::dev());
        let h = open_ok(&manager, c.clone(), &types).await;
        let slow = "SELECT * FROM slow_numbers(100000, 50)";
        let (events, summary) = collect_run(
            exec.run(
                &h,
                RunRequest::statement(slow)
                    .with_timeout(TimeoutChoice::After(Duration::from_millis(150))),
            ),
        )
        .await;
        assert_eq!(summary.end, RunEnd::TimedOut { index: 0 });
        assert!(events.iter().any(|e| matches!(
            e,
            RunEvent::StatementTimedOut { after, .. } if *after == Duration::from_millis(150)
        )));

        // The connection's default timeout from the preferences applies to `Default`…
        let mut prefs = exec.preferences();
        PreferenceChange::ConnectionTimeout(c.id, Some(Duration::from_secs(1)))
            .apply(&mut prefs)
            .unwrap();
        exec.set_preferences(prefs);
        let (_, summary) = collect_run(exec.run(&h, RunRequest::statement(slow))).await;
        assert_eq!(summary.end, RunEnd::TimedOut { index: 0 });
        // …and `None` overrides it.
        let (_, summary) = collect_run(
            exec.run(
                &h,
                RunRequest::statement("SELECT * FROM slow_numbers(400, 50)")
                    .with_timeout(TimeoutChoice::None),
            ),
        )
        .await;
        assert_eq!(summary.end, RunEnd::Completed);
    });
    manager.shutdown(Duration::from_secs(5));
}

/// Exit criterion of phase 3: a `DELETE` without `WHERE` on a production connection asks for
/// confirmation; statements before it are not run again when the script resumes.
#[test]
fn production_scripts_stop_for_confirmation_and_resume() {
    let tmp = tempfile::tempdir().unwrap();
    let (manager, _) = manager(Arc::new(mock_driver()), tmp.path());
    let types = ConnectionTypes::default();
    let exec = service(&manager, None);
    ui(async {
        let h = open_ok(&manager, mock_connection(ConnectionTypeId::prod()), &types).await;
        let script = "SELECT * FROM numbers(1);\nDELETE FROM t;\nSELECT * FROM numbers(2)";
        let (events, summary) = collect_run(exec.run(&h, RunRequest::script(script))).await;
        assert_eq!(summary.end, RunEnd::NeedsConfirmation { index: 1 });
        assert_eq!(summary.executed, 1);
        let request = events
            .iter()
            .find_map(|e| match e {
                RunEvent::NeedsConfirmation { index: 1, request } => Some(request.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(request.statement, "DELETE FROM t");
        assert!(request.production);
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, RunEvent::StatementStarted { index: 1, .. }))
        );

        let (events, summary) = collect_run(
            exec.run(
                &h,
                RunRequest::script(script)
                    .resume_at(1)
                    .confirmed(1, request.token),
            ),
        )
        .await;
        assert_eq!(summary.end, RunEnd::Completed);
        assert_eq!(summary.executed, 2);
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, RunEvent::StatementStarted { index: 0, .. }))
        );
        assert!(events.iter().any(|e| matches!(
            e,
            RunEvent::Item {
                index: 1,
                item: ResultItem::UpdateCount(Some(1))
            }
        )));
        // The token was single-use.
        let (_, summary) = collect_run(
            exec.run(
                &h,
                RunRequest::script(script)
                    .resume_at(1)
                    .confirmed(1, request.token),
            ),
        )
        .await;
        assert_eq!(summary.end, RunEnd::Failed { index: 1 });
    });
    manager.shutdown(Duration::from_secs(5));
}

#[test]
fn pending_transactions_are_tracked_and_guarded() {
    let tmp = tempfile::tempdir().unwrap();
    let (manager, _) = manager(Arc::new(mock_driver()), tmp.path());
    let mut events = manager.take_events().unwrap();
    let types = ConnectionTypes::default();
    let exec = service(&manager, None);
    ui(async {
        // Production: manual commit (ADR-0007 §5); a dev connection can ask for it too.
        let h = open_ok(&manager, mock_connection(ConnectionTypeId::prod()), &types).await;
        assert!(!h.info().auto_commit);
        assert!(!h.transaction_state().in_transaction);
        let mut dev = mock_connection(ConnectionTypeId::dev());
        dev.auto_commit = Some(false);
        let d = open_ok(&manager, dev, &types).await;
        assert!(!d.info().auto_commit);

        let (_, summary) =
            collect_run(exec.run(&h, RunRequest::statement("INSERT INTO t VALUES (1)"))).await;
        assert!(summary.transaction.in_transaction, "{summary:?}");
        assert!(h.transaction_state().in_transaction);
        let changed = loop {
            match tokio::time::timeout(Duration::from_secs(5), events.recv())
                .await
                .unwrap()
            {
                Some(CoreEvent::TransactionChanged { session, state, .. }) if session == h.id() => {
                    break state;
                }
                Some(_) => {}
                None => panic!("events ended"),
            }
        };
        assert!(changed.in_transaction);
        assert_eq!(manager.sessions_with_open_transaction().len(), 1);

        // Turning auto-commit on would end the transaction implicitly: refused.
        assert!(matches!(
            h.set_auto_commit(true).await.unwrap(),
            Err(CoreError::PendingTransaction)
        ));
        // Closing asks first: always for production, even when the preference is off.
        let off = {
            let mut p = Preferences::default();
            PreferenceChange::CloseWithOpenTransaction(false)
                .apply(&mut p)
                .unwrap();
            p.confirmations
        };
        assert!(h.close_needs_confirmation(&off));
        assert!(h.close_needs_confirmation(&ConfirmationPreferences::default()));
        let st = h.rollback().await.unwrap().unwrap();
        assert!(!st.in_transaction);
        assert!(!h.close_needs_confirmation(&ConfirmationPreferences::default()));

        let (_, summary) =
            collect_run(exec.run(&d, RunRequest::statement("INSERT INTO t VALUES (2)"))).await;
        assert!(summary.transaction.in_transaction);
        assert!(!d.close_needs_confirmation(&off), "dev, preference off");
        assert!(d.close_needs_confirmation(&ConfirmationPreferences::default()));
        d.close_with(TransactionEnd::Commit).await.unwrap();
        assert!(
            !d.transaction_state().in_transaction,
            "committed before closing"
        );
        h.close_with(TransactionEnd::Rollback).await.unwrap();
    });
    manager.shutdown(Duration::from_secs(5));
}

// ---------------------------------------------------------------------------------------------
// A driver whose connects can fail and whose sessions can "die" (keep-alive, reconnect).

#[derive(Clone)]
struct Flaky {
    mock: MockDriver,
    fail_connects: Arc<AtomicU32>,
    connects: Arc<AtomicU32>,
    dead: Arc<AtomicBool>,
    pings: Arc<AtomicU64>,
}

impl Flaky {
    fn new(mock: MockDriver) -> Self {
        Self {
            mock,
            fail_connects: Arc::default(),
            connects: Arc::default(),
            dead: Arc::default(),
            pings: Arc::default(),
        }
    }
}

#[async_trait::async_trait]
impl Driver for Flaky {
    fn family(&self) -> &'static str {
        "flaky-mock"
    }
    fn engines(&self) -> &[api::EngineId] {
        self.mock.engines()
    }
    fn capabilities(&self, engine: &api::EngineId, v: Option<&str>) -> DriverResult<Capabilities> {
        self.mock.capabilities(engine, v)
    }
    fn dialect(&self, engine: &api::EngineId) -> DriverResult<Arc<dyn Dialect>> {
        self.mock.dialect(engine)
    }
    async fn connect(&self, params: ConnectParams) -> DriverResult<Box<dyn Session>> {
        self.connects.fetch_add(1, Ordering::SeqCst);
        let mut left = self.fail_connects.load(Ordering::SeqCst);
        let failing = loop {
            if left == 0 {
                break false;
            }
            match self.fail_connects.compare_exchange(
                left,
                left - 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => break true,
                Err(now) => left = now,
            }
        };
        if failing {
            return Err(DriverError::Connect("connection refused".into()));
        }
        let inner = self.mock.connect(params).await?;
        Ok(Box::new(FlakySession {
            inner,
            dead: Arc::clone(&self.dead),
            pings: Arc::clone(&self.pings),
        }))
    }
}

struct FlakySession {
    inner: Box<dyn Session>,
    dead: Arc<AtomicBool>,
    pings: Arc<AtomicU64>,
}

#[async_trait::async_trait]
impl Session for FlakySession {
    fn capabilities(&self) -> &Capabilities {
        self.inner.capabilities()
    }
    fn server_version(&self) -> Option<&str> {
        self.inner.server_version()
    }
    fn dialect(&self) -> Arc<dyn Dialect> {
        self.inner.dialect()
    }
    fn metadata(&self) -> Arc<dyn Metadata> {
        self.inner.metadata()
    }
    async fn execute(&mut self, s: Statement, o: ExecOptions) -> DriverResult<ResultStream> {
        if s.text.contains("broken_stream()") {
            // Two pages of rows, then an engine error (not a cancel).
            return Ok(ResultStream::from_items(broken_stream_items(), &o));
        }
        self.inner.execute(s, o).await
    }
    fn cancel_handle(&self) -> Arc<dyn CancelHandle> {
        self.inner.cancel_handle()
    }
    fn auto_commit(&self) -> bool {
        self.inner.auto_commit()
    }
    fn in_transaction(&self) -> bool {
        self.inner.in_transaction()
    }
    async fn set_auto_commit(&mut self, on: bool) -> DriverResult<()> {
        self.inner.set_auto_commit(on).await
    }
    fn is_read_only(&self) -> bool {
        self.inner.is_read_only()
    }
    async fn set_read_only(&mut self, ro: bool) -> DriverResult<()> {
        self.inner.set_read_only(ro).await
    }
    async fn ping(&mut self) -> DriverResult<()> {
        self.pings.fetch_add(1, Ordering::SeqCst);
        if self.dead.load(Ordering::SeqCst) {
            return Err(DriverError::Closed("server closed the connection".into()));
        }
        self.inner.ping().await
    }
    async fn close(self: Box<Self>) -> DriverResult<()> {
        self.inner.close().await
    }
}

fn broken_stream_items() -> Vec<DriverResult<ResultItem>> {
    use ninesql_core::driver_api::arrow_array::{Int64Array, RecordBatch};
    use ninesql_core::driver_api::arrow_schema::{DataType, Field, Schema};
    let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, false)]));
    let batch = |from: i64| {
        RecordBatch::try_new(
            Arc::clone(&schema),
            vec![Arc::new(Int64Array::from_iter_values(from..from + 100))],
        )
        .unwrap()
    };
    vec![
        Ok(ResultItem::ResultSet(api::ResultSetHeader::tabular(
            0,
            Arc::clone(&schema),
        ))),
        Ok(ResultItem::Rows(batch(0))),
        Ok(ResultItem::Rows(batch(100))),
        Err(DriverError::Query {
            code: Some("XX000".into()),
            message: "disk read error".into(),
            position: None,
        }),
    ]
}

/// Review M1: an engine error that arrives around a release is a failure, never a closed
/// (successful) statement; only a confirmed cancel is `StatementClosed`.
#[test]
fn an_error_after_a_release_is_reported_as_a_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let (manager, _) = manager(Arc::new(Flaky::new(mock_driver())), tmp.path());
    let types = ConnectionTypes::default();
    let exec = service(&manager, None);
    ui(async {
        let h = open_ok(&manager, mock_connection(ConnectionTypeId::dev()), &types).await;
        let mut run = exec.run(
            &h,
            RunRequest::statement("SELECT broken_stream()")
                .with_budget(FetchBudget::default().with_batch_rows(100))
                .with_paging(100),
        );
        let mut n = 0;
        until(&mut run, &mut n, |e| matches!(e, RunEvent::PageEnd { .. })).await;
        // The session task queued the engine error meanwhile.
        tokio::time::sleep(Duration::from_millis(200)).await;
        run.release(0);
        let e = until(&mut run, &mut n, |e| {
            matches!(
                e,
                RunEvent::StatementClosed { .. } | RunEvent::StatementFailed { .. }
            )
        })
        .await;
        assert!(
            matches!(
                e,
                RunEvent::StatementFailed {
                    error: CoreError::Driver(DriverError::Query { .. }),
                    ..
                }
            ),
            "{e:?}"
        );
        let e = until(&mut run, &mut n, |e| matches!(e, RunEvent::Done(_))).await;
        assert!(matches!(
            e,
            RunEvent::Done(RunSummary {
                end: RunEnd::Failed { index: 0 },
                ..
            })
        ));
    });
    manager.shutdown(Duration::from_secs(5));
}

/// Review M1: `release` is ignored for a statement that is not a read; it streams to its end.
#[test]
fn non_reads_are_neither_paged_nor_released() {
    let tmp = tempfile::tempdir().unwrap();
    let (manager, _) = manager(Arc::new(mock_driver()), tmp.path());
    let types = ConnectionTypes::default();
    let exec = service(&manager, None);
    ui(async {
        let h = open_ok(&manager, mock_connection(ConnectionTypeId::dev()), &types).await;
        let run = exec.run(
            &h,
            RunRequest::script("INSERT INTO t VALUES (1);\nINSERT INTO t VALUES (2)")
                .with_paging(1),
        );
        run.release(0);
        let (events, summary) = collect_run(run).await;
        assert!(events.iter().any(|e| matches!(
            e,
            RunEvent::StatementFinished {
                index: 0,
                update_count: Some(1),
                ..
            }
        )));
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, RunEvent::StatementClosed { index: 0, .. }))
        );
        assert_eq!(summary.end, RunEnd::Completed);
    });
    manager.shutdown(Duration::from_secs(5));
}

#[test]
fn keep_alive_pings_idle_sessions_and_detects_a_lost_connection() {
    let tmp = tempfile::tempdir().unwrap();
    let flaky = Flaky::new(mock_driver());
    let (manager, _) = manager(Arc::new(flaky.clone()), tmp.path());
    let mut events = manager.take_events().unwrap();
    let types = ConnectionTypes::default();
    ui(async {
        let h = open_ok(&manager, mock_connection(ConnectionTypeId::dev()), &types).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(flaky.pings.load(Ordering::SeqCst), 0, "off by default");
        manager.set_keep_alive(Some(Duration::from_millis(50)));
        assert_eq!(manager.keep_alive(), Some(Duration::from_millis(50)));
        // The new interval is read at the next loop turn: a command wakes the task.
        let _ = h.ping().await;
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(
            flaky.pings.load(Ordering::SeqCst) >= 3,
            "idle session pinged"
        );
        flaky.dead.store(true, Ordering::SeqCst);
        let reason = loop {
            match tokio::time::timeout(Duration::from_secs(5), events.recv())
                .await
                .unwrap()
            {
                Some(CoreEvent::SessionClosed {
                    session, reason, ..
                }) if session == h.id() => {
                    break reason;
                }
                Some(_) => {}
                None => panic!("events ended"),
            }
        };
        assert!(
            matches!(reason, CloseReason::Lost(DriverError::Closed(_))),
            "{reason:?}"
        );
        assert!(h.is_closed());
    });
    manager.shutdown(Duration::from_secs(5));
}

async fn reconnect_to_end(
    mut r: ninesql_core::Reconnect,
) -> (Vec<ReconnectEvent>, Result<OpenOutcome, CoreError>) {
    let mut seen = Vec::new();
    loop {
        match tokio::time::timeout(Duration::from_secs(20), r.next())
            .await
            .unwrap()
        {
            Some(ReconnectEvent::Done(outcome)) => return (seen, outcome),
            Some(e) => seen.push(e),
            None => panic!("reconnect ended without Done"),
        }
    }
}

#[test]
fn reconnect_retries_transient_failures_with_backoff_only() {
    let tmp = tempfile::tempdir().unwrap();
    let flaky = Flaky::new(mock_driver());
    let (manager, _) = manager(Arc::new(flaky.clone()), tmp.path());
    let types = ConnectionTypes::default();
    let fast = ReconnectPolicy::with_attempts(3)
        .with_delays(Duration::from_millis(10), Duration::from_millis(15));
    assert_eq!(fast.delay_after(1), Duration::from_millis(10));
    assert_eq!(fast.delay_after(2), Duration::from_millis(15), "capped");
    let default = ReconnectPolicy::default();
    assert_eq!(default.delay_after(3), Duration::from_secs(4));
    assert_eq!(default.delay_after(30), Duration::from_secs(30));
    ui(async {
        let c = mock_connection(ConnectionTypeId::dev());
        // Two refused connects, then success on the third attempt.
        flaky.fail_connects.store(2, Ordering::SeqCst);
        let (seen, outcome) =
            reconnect_to_end(manager.reconnect(OpenRequest::new(c.clone(), &types), fast)).await;
        let attempts = seen
            .iter()
            .filter(|e| matches!(e, ReconnectEvent::Attempt { .. }))
            .count();
        let retries = seen
            .iter()
            .filter(|e| matches!(e, ReconnectEvent::RetryIn { .. }))
            .count();
        assert_eq!((attempts, retries), (3, 2));
        let h = match outcome.unwrap() {
            OpenOutcome::Opened(h) => h,
            other => panic!("{other:?}"),
        };
        // A fresh session: nothing was replayed.
        assert!(!h.transaction_state().in_transaction);
        h.close().await.unwrap().unwrap();

        // Out of attempts: the last error.
        flaky.fail_connects.store(5, Ordering::SeqCst);
        let (_, outcome) = reconnect_to_end(
            manager.reconnect(
                OpenRequest::new(c.clone(), &types),
                ReconnectPolicy::with_attempts(2)
                    .with_delays(Duration::from_millis(5), Duration::from_millis(5)),
            ),
        )
        .await;
        assert!(matches!(
            outcome,
            Err(CoreError::Driver(DriverError::Connect(_)))
        ));
        flaky.fail_connects.store(0, Ordering::SeqCst);

        // Init SQL that needs confirmation is asked again: nothing reaches the driver.
        let mut prod = mock_connection(ConnectionTypeId::prod());
        prod.init_sql = vec!["DROP TABLE scratch".into()];
        let before = flaky.connects.load(Ordering::SeqCst);
        let (seen, outcome) =
            reconnect_to_end(manager.reconnect(OpenRequest::new(prod.clone(), &types), fast)).await;
        assert_eq!(seen.len(), 1, "one attempt");
        let confirmation = match outcome.unwrap() {
            OpenOutcome::InitSqlNeedsConfirmation(c) => c,
            other => panic!("{other:?}"),
        };
        assert_eq!(flaky.connects.load(Ordering::SeqCst), before);
        // With the token, it reconnects and runs it once.
        let (_, outcome) = reconnect_to_end(manager.reconnect(
            OpenRequest::new(prod, &types).with_init_sql_confirmation(confirmation.token),
            fast,
        ))
        .await;
        assert!(matches!(outcome, Ok(OpenOutcome::Opened(_))));
    });
    manager.shutdown(Duration::from_secs(5));
}

#[test]
fn reconnect_does_not_retry_an_authentication_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let mock = mock_driver().with_password(Secret::new("right-pw".to_owned()));
    let flaky = Flaky::new(mock);
    let (manager, _) = manager(Arc::new(flaky.clone()), tmp.path());
    let types = ConnectionTypes::default();
    ui(async {
        let c = mock_connection(ConnectionTypeId::dev());
        let (seen, outcome) = reconnect_to_end(
            manager.reconnect(
                OpenRequest::new(c, &types),
                ReconnectPolicy::with_attempts(5)
                    .with_delays(Duration::from_millis(5), Duration::from_millis(5)),
            ),
        )
        .await;
        assert_eq!(seen.len(), 1);
        assert!(
            matches!(outcome, Err(CoreError::Driver(DriverError::Auth(_)))),
            "{outcome:?}"
        );
        assert_eq!(flaky.connects.load(Ordering::SeqCst), 1);
    });
    manager.shutdown(Duration::from_secs(5));
}

// ---------------------------------------------------------------------------------------------
// History (F-SQL-08, I1)

fn all_bytes(dir: &std::path::Path) -> Vec<u8> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_file() {
            out.extend(std::fs::read(&path).unwrap());
        }
    }
    out
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|w| w == needle.as_bytes())
}

#[test]
fn history_records_text_never_parameter_values_and_respects_types() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("state");
    let db = HistoryDb::open(&state.join("history.sqlite")).unwrap();
    let history = History::start(db, "project-1", &Preferences::default().history).unwrap();
    let (manager, _) = manager(Arc::new(mock_driver()), tmp.path());
    let mut types = ConnectionTypes::default();
    let mut quiet = ConnectionType::test();
    quiet.id = ConnectionTypeId::new("quiet").unwrap();
    quiet.log_query_text = false;
    types.upsert(quiet.clone());
    let exec = service(&manager, Some(history.clone()));
    let param_secret = "s3cret-bound-param-value";
    let literal = "typed-literal-pw-9137";
    ui(async {
        let dev = mock_connection(ConnectionTypeId::dev());
        let h = open_ok(&manager, dev.clone(), &types).await;
        let (_, s) = collect_run(
            exec.run(
                &h,
                RunRequest::statement("SELECT * FROM numbers(2)")
                    .with_params(vec![Param::Text(param_secret.to_owned())]),
            ),
        )
        .await;
        assert_eq!(s.end, RunEnd::Completed, "{s:?}");
        // Review M3: a statement with a credential word is never recorded (I1).
        let (_, s) = collect_run(exec.run(
            &h,
            RunRequest::statement(format!("ALTER USER bob PASSWORD '{literal}'")),
        ))
        .await;
        assert_eq!(s.end, RunEnd::Completed);
        let (_, s) = collect_run(exec.run(
            &h,
            RunRequest::statement(format!(
                "CREATE USER MAPPING FOR bob SERVER s OPTIONS (user 'u', password '{literal}-2')"
            )),
        ))
        .await;
        assert_eq!(s.end, RunEnd::Completed);
        let (_, s) = collect_run(exec.run(&h, RunRequest::statement("SELECT error()"))).await;
        assert_eq!(s.end, RunEnd::Failed { index: 0 });

        // Production and a type without query logging: nothing at all.
        let p = open_ok(&manager, mock_connection(ConnectionTypeId::prod()), &types).await;
        let (_, s) = collect_run(exec.run(
            &p,
            RunRequest::statement("SELECT * FROM numbers(1) -- prod-marker"),
        ))
        .await;
        assert_eq!(s.end, RunEnd::Completed);
        let q = open_ok(&manager, mock_connection(quiet.id.clone()), &types).await;
        let (_, s) = collect_run(exec.run(
            &q,
            RunRequest::statement("SELECT * FROM numbers(1) -- quiet-marker"),
        ))
        .await;
        assert_eq!(s.end, RunEnd::Completed);

        // The query runs after every queued insert; the bytes are read before any checkpoint,
        // so the write-ahead log is checked too.
        let entries = history
            .query(HistoryQuery::recent(100))
            .await
            .unwrap()
            .unwrap();
        let before_checkpoint = all_bytes(&state);
        assert!(
            contains(&before_checkpoint, "numbers(2)"),
            "entries are on disk"
        );
        assert!(
            !contains(&before_checkpoint, literal),
            "I1: a password literal reached history"
        );
        assert!(!contains(&before_checkpoint, param_secret));
        assert_eq!(entries.len(), 2, "{entries:?}");
        assert!(entries.iter().all(|e| e.connection == dev.id.to_string()));
        assert!(entries.iter().all(|e| e.project == "project-1"));
        let select = entries
            .iter()
            .find(|e| e.statement == "SELECT * FROM numbers(2)")
            .unwrap();
        assert_eq!(select.param_count, 1);
        assert_eq!(select.rows, Some(2));
        assert_eq!(select.status, EntryStatus::Succeeded);
        let failed = entries
            .iter()
            .find(|e| e.status == EntryStatus::Failed)
            .unwrap();
        assert_eq!(failed.error_kind.as_deref(), Some("driver-query"));
        assert_eq!(failed.error_code.as_deref(), Some("XX000"));
        assert!(!entries.iter().any(|e| e.statement.contains(literal)));
        let recent = history.recent(dev.id, 10).await.unwrap().unwrap();
        assert_eq!(recent.len(), 2);
    });
    manager.shutdown(Duration::from_secs(5));
    drop(exec);
    drop(history);
    // The writer checkpoints and closes once the last clone is gone.
    std::thread::sleep(Duration::from_millis(300));
    let bytes = all_bytes(&state);
    assert!(!bytes.is_empty());
    assert!(
        !contains(&bytes, literal),
        "I1: a password literal reached history"
    );
    assert!(
        !contains(&bytes, param_secret),
        "I1: a bound parameter value reached history"
    );
    assert!(
        !contains(&bytes, "prod-marker"),
        "production statement recorded"
    );
    assert!(
        !contains(&bytes, "quiet-marker"),
        "log_query_text = false statement recorded"
    );
}

#[test]
fn history_retention_and_disable_apply_from_preferences() {
    let tmp = tempfile::tempdir().unwrap();
    let db = HistoryDb::open(&tmp.path().join("h/history.sqlite")).unwrap();
    let history = History::start(db, "p", &Preferences::default().history).unwrap();
    let (manager, _) = manager(Arc::new(mock_driver()), tmp.path());
    let types = ConnectionTypes::default();
    let exec = service(&manager, Some(history.clone()));
    ui(async {
        let h = open_ok(&manager, mock_connection(ConnectionTypeId::dev()), &types).await;
        for i in 0..5 {
            let _ =
                collect_run(exec.run(&h, RunRequest::statement(format!("SELECT 1 AS c{i}")))).await;
        }
        let mut prefs = exec.preferences();
        PreferenceChange::HistoryMaxEntries(2)
            .apply(&mut prefs)
            .unwrap();
        exec.set_preferences(prefs.clone());
        history.flush().await.unwrap().unwrap();
        let left = history
            .query(HistoryQuery::recent(10))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(left.len(), 2);
        assert_eq!(left[0].statement, "SELECT 1 AS c4");

        PreferenceChange::HistoryEnabled(false)
            .apply(&mut prefs)
            .unwrap();
        exec.set_preferences(prefs);
        let _ = collect_run(exec.run(&h, RunRequest::statement("SELECT 1 AS off"))).await;
        history.flush().await.unwrap().unwrap();
        let left = history
            .query(HistoryQuery::recent(10))
            .await
            .unwrap()
            .unwrap();
        assert!(left.iter().all(|e| e.statement != "SELECT 1 AS off"));
        assert_eq!(history.clear().await.unwrap().unwrap(), 2);
    });
    manager.shutdown(Duration::from_secs(5));
}

// ---------------------------------------------------------------------------------------------
// Paging for the grid (P3-B contract)

/// Reads events until `stop` matches one (returned), collecting the rows of `index`.
async fn until(
    run: &mut ninesql_core::Run,
    rows: &mut usize,
    stop: impl Fn(&RunEvent) -> bool,
) -> RunEvent {
    loop {
        let e = tokio::time::timeout(Duration::from_secs(10), run.next())
            .await
            .expect("paging timed out")
            .expect("run ended");
        if let RunEvent::Item {
            item: ResultItem::Rows(b),
            ..
        } = &e
        {
            *rows += b.num_rows();
        }
        if stop(&e) {
            return e;
        }
    }
}

#[test]
fn paging_delivers_the_first_page_then_waits_for_fetch_or_release() {
    let tmp = tempfile::tempdir().unwrap();
    let mock = mock_driver();
    let stats = mock.stats();
    let (manager, _) = manager(Arc::new(mock), tmp.path());
    let types = ConnectionTypes::default();
    let exec = service(&manager, None);
    let budget = FetchBudget::default().with_batch_rows(50);
    ui(async {
        let h = open_ok(&manager, mock_connection(ConnectionTypeId::dev()), &types).await;
        // A script: the first statement is closed after its first page, the last one pauses.
        let mut run = exec.run(
            &h,
            RunRequest::script("SELECT * FROM numbers(100000);\nSELECT * FROM numbers(1000)")
                .with_budget(budget)
                .with_paging(100),
        );
        assert!(matches!(
            run.next().await,
            Some(RunEvent::Started { statements: 2 })
        ));
        let mut first = 0;
        let e = until(&mut run, &mut first, |e| {
            matches!(e, RunEvent::StatementClosed { index: 0, .. })
        })
        .await;
        assert_eq!(first, 100);
        assert!(matches!(e, RunEvent::StatementClosed { rows: 100, .. }));
        assert!(
            stats.rows_generated() < 100_000,
            "stream closed, not drained"
        );

        let mut second = 0;
        until(&mut run, &mut second, |e| {
            matches!(
                e,
                RunEvent::PageEnd {
                    index: 1,
                    rows: 100
                }
            )
        })
        .await;
        assert_eq!(second, 100);
        // Paused: nothing more comes until asked.
        assert!(
            tokio::time::timeout(Duration::from_millis(200), run.next())
                .await
                .is_err()
        );
        let control = run.control();
        control.fetch(0, FetchMode::All); // not the running statement: ignored
        control.fetch(1, FetchMode::Page(200));
        until(&mut run, &mut second, |e| {
            matches!(e, RunEvent::PageEnd { index: 1, .. })
        })
        .await;
        assert_eq!(second, 300);
        control.fetch(1, FetchMode::All);
        until(&mut run, &mut second, |e| {
            matches!(
                e,
                RunEvent::StatementFinished {
                    index: 1,
                    rows: 1000,
                    ..
                }
            )
        })
        .await;
        assert_eq!(second, 1000);
        let Some(RunEvent::Done(summary)) = run.next().await else {
            panic!("no Done");
        };
        assert_eq!(summary.end, RunEnd::Completed);
        assert_eq!(summary.succeeded, 2);

        // Release while paused: closed early, the session is free again.
        let mut run = exec.run(
            &h,
            RunRequest::statement("SELECT * FROM numbers(100000)")
                .with_budget(budget)
                .with_paging(100),
        );
        let mut n = 0;
        until(&mut run, &mut n, |e| matches!(e, RunEvent::PageEnd { .. })).await;
        run.release(0);
        until(&mut run, &mut n, |e| {
            matches!(
                e,
                RunEvent::StatementClosed {
                    index: 0,
                    rows: 100
                }
            )
        })
        .await;
        let Some(RunEvent::Done(summary)) = run.next().await else {
            panic!("no Done");
        };
        assert_eq!(summary.end, RunEnd::Completed);
        let (_, s) =
            collect_run(exec.run(&h, RunRequest::statement("SELECT * FROM numbers(3)"))).await;
        assert_eq!(s.end, RunEnd::Completed);

        // "Fetch all" stays under the budget: the result ends with `Truncated`.
        let mut run = exec.run(
            &h,
            RunRequest::statement("SELECT * FROM numbers(1000000)")
                .with_budget(budget.with_max_rows(Some(500)))
                .with_paging(100),
        );
        let mut n = 0;
        until(&mut run, &mut n, |e| matches!(e, RunEvent::PageEnd { .. })).await;
        run.fetch(0, FetchMode::All);
        let mut truncated = false;
        loop {
            match tokio::time::timeout(Duration::from_secs(10), run.next())
                .await
                .unwrap()
            {
                Some(RunEvent::Item {
                    item: ResultItem::Rows(b),
                    ..
                }) => n += b.num_rows(),
                Some(RunEvent::Item {
                    item: ResultItem::Truncated(_),
                    ..
                }) => truncated = true,
                Some(RunEvent::Done(_)) => break,
                Some(_) => {}
                None => panic!("no Done"),
            }
        }
        assert!(truncated);
        assert_eq!(n, 500);

        // Cancel while paused ends the run as cancelled.
        let mut run = exec.run(
            &h,
            RunRequest::statement("SELECT * FROM numbers(100000)")
                .with_budget(budget)
                .with_paging(100),
        );
        let mut n = 0;
        until(&mut run, &mut n, |e| matches!(e, RunEvent::PageEnd { .. })).await;
        run.control().cancel();
        let e = until(&mut run, &mut n, |e| matches!(e, RunEvent::Done(_))).await;
        assert!(matches!(
            e,
            RunEvent::Done(RunSummary {
                end: RunEnd::Cancelled { index: 0 },
                ..
            })
        ));
    });
    manager.shutdown(Duration::from_secs(5));
}

/// Review M2: a secrets lock ends a reconnect that holds prompted secrets, and the policy is
/// clamped.
#[test]
fn a_secrets_lock_stops_a_reconnect_holding_prompted_secrets() {
    use ninesql_core::{Auth, SecretSlot, SecretSource, SecretsLockReason};
    let clamped = ReconnectPolicy::with_attempts(1_000)
        .with_delays(Duration::from_secs(600), Duration::from_secs(6_000))
        .clamped();
    assert_eq!(clamped.attempts, ninesql_core::MAX_RECONNECT_ATTEMPTS);
    assert_eq!(clamped.max_delay, ninesql_core::MAX_RECONNECT_DELAY);
    assert_eq!(clamped.initial_delay, ninesql_core::MAX_RECONNECT_DELAY);
    assert_eq!(ReconnectPolicy::with_attempts(0).clamped().attempts, 1);

    let tmp = tempfile::tempdir().unwrap();
    let flaky = Flaky::new(mock_driver().with_password(Secret::new("right-pw".to_owned())));
    let (manager, _) = manager(Arc::new(flaky.clone()), tmp.path());
    let types = ConnectionTypes::default();
    ui(async {
        let mut c = mock_connection(ConnectionTypeId::dev());
        c.auth = Auth::Password {
            username: "nine".into(),
            secret: SecretSource::Prompt,
        };
        flaky.fail_connects.store(100, Ordering::SeqCst);
        let request = OpenRequest::new(c, &types).with_secret(
            SecretSlot::Auth,
            ninesql_secrets::SecretString::from("right-pw".to_owned()),
        );
        let mut r = manager.reconnect(
            request,
            ReconnectPolicy::with_attempts(10)
                .with_delays(Duration::from_secs(2), Duration::from_secs(2)),
        );
        loop {
            match tokio::time::timeout(Duration::from_secs(10), r.next())
                .await
                .unwrap()
            {
                Some(ReconnectEvent::RetryIn { .. }) => break,
                Some(_) => {}
                None => panic!("ended early"),
            }
        }
        // "Lock secrets now" (or sleep, screen lock, idle lock) while it waits.
        manager.notify_secrets_locked(SecretsLockReason::Requested);
        let (_, outcome) = reconnect_to_end(r).await;
        match outcome {
            Ok(OpenOutcome::NeedsSecrets(prompts)) => {
                assert_eq!(prompts.len(), 1, "the password is asked again");
            }
            other => panic!("{other:?}"),
        }
        let connects = flaky.connects.load(Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            flaky.connects.load(Ordering::SeqCst),
            connects,
            "no further attempt"
        );
    });
    manager.shutdown(Duration::from_secs(5));

    // Without prompted secrets a lock does not stop it (nothing secret is held).
    let open_flaky = Flaky::new(mock_driver());
    let (manager, _) = support::manager(Arc::new(open_flaky.clone()), tmp.path());
    ui(async {
        open_flaky.fail_connects.store(1, Ordering::SeqCst);
        let mut r = manager.reconnect(
            OpenRequest::new(mock_connection(ConnectionTypeId::dev()), &types),
            ReconnectPolicy::with_attempts(3)
                .with_delays(Duration::from_millis(200), Duration::from_millis(200)),
        );
        loop {
            match tokio::time::timeout(Duration::from_secs(10), r.next())
                .await
                .unwrap()
            {
                Some(ReconnectEvent::RetryIn { .. }) => break,
                Some(_) => {}
                None => panic!("ended early"),
            }
        }
        manager.notify_secrets_locked(SecretsLockReason::Idle);
        let (_, outcome) = reconnect_to_end(r).await;
        assert!(matches!(outcome, Ok(OpenOutcome::Opened(_))), "{outcome:?}");
    });
    manager.shutdown(Duration::from_secs(5));
}
