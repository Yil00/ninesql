//! Contract tests of the driver API, run against the mock driver: streaming under a budget,
//! truncation, cancel, timeout, read-only refusal of writes, busy sessions, metadata paging.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use ninesql_driver_api::conformance;
use ninesql_driver_api::mock::{MOCK_ENGINE, MOCK_TABLE_COUNT, MockDriver};
use ninesql_driver_api::{
    Auth, CancelToken, Capabilities, ConnectParams, Driver, DriverError, Endpoint, ExecOptions,
    Feature, FetchBudget, ObjectKind, ObjectPath, PageRequest, PropertyValue, ReadOnlyMechanism,
    ResultItem, ResultSetKind, ResultStream, Secret, Session, Statement,
    TRUNCATED_VALUES_METADATA_KEY, TruncationReason,
};

fn params() -> ConnectParams {
    ConnectParams::new(MOCK_ENGINE, Endpoint::InMemory)
}

async fn connect(driver: &MockDriver) -> Box<dyn Session> {
    driver.connect(params()).await.unwrap()
}

async fn connect_read_only(driver: &MockDriver) -> Box<dyn Session> {
    let mut p = params();
    p.read_only = true;
    driver.connect(p).await.unwrap()
}

/// Drains a stream; returns (rows, items, terminal error).
async fn drain(stream: &mut ResultStream) -> (u64, Vec<ResultItem>, Option<DriverError>) {
    let mut rows = 0u64;
    let mut items = Vec::new();
    while let Some(item) = stream.next().await {
        match item {
            Ok(item) => {
                match &item {
                    ResultItem::Rows(b) => rows += b.num_rows() as u64,
                    ResultItem::Documents(d) => rows += d.len() as u64,
                    _ => {}
                }
                items.push(item);
            }
            Err(e) => return (rows, items, Some(e)),
        }
    }
    (rows, items, None)
}

fn opts(budget: FetchBudget) -> ExecOptions {
    ExecOptions::default().with_budget(budget)
}

#[tokio::test]
async fn streams_all_rows_in_batches_within_budget() {
    let driver = MockDriver::new();
    let mut s = connect(&driver).await;
    let mut stream = s
        .execute(
            Statement::sql("SELECT * FROM numbers(1000)"),
            ExecOptions::default(),
        )
        .await
        .unwrap();
    let (rows, items, err) = drain(&mut stream).await;
    assert!(err.is_none());
    assert_eq!(rows, 1000);
    assert!(matches!(
        &items[0],
        ResultItem::ResultSet(h) if matches!(h.kind, ResultSetKind::Tabular(_))
    ));
    // Default batch size is 200 rows: header + 5 batches.
    assert_eq!(items.len(), 6);
    assert!(
        items[1..]
            .iter()
            .all(|i| matches!(i, ResultItem::Rows(b) if b.num_rows() == 200))
    );
    assert!(!items.iter().any(|i| matches!(i, ResultItem::Truncated(_))));
    assert!(stream.is_finished());
}

#[tokio::test]
async fn truncates_at_max_rows_and_stops_producing() {
    let driver = MockDriver::new();
    let stats = driver.stats();
    let mut s = connect(&driver).await;
    let budget = FetchBudget::default()
        .with_batch_rows(100)
        .with_max_rows(Some(250));
    let mut stream = s
        .execute(
            Statement::sql("SELECT * FROM numbers(1000000)"),
            opts(budget),
        )
        .await
        .unwrap();
    let (rows, items, err) = drain(&mut stream).await;
    assert!(err.is_none());
    assert_eq!(rows, 250);
    let Some(ResultItem::Truncated(t)) = items.last() else {
        panic!("expected Truncated, got {items:?}");
    };
    assert_eq!(t.reason, TruncationReason::MaxRows);
    assert_eq!(t.rows_delivered, 250);
    // The server produced only the batches that were pulled (3 x 100), not one million rows.
    assert_eq!(stats.rows_generated(), 300);
}

#[tokio::test]
async fn truncates_at_max_bytes() {
    let driver = MockDriver::new();
    let stats = driver.stats();
    let mut s = connect(&driver).await;
    let max_bytes = 64 * 1024;
    let budget = FetchBudget::default()
        .with_batch_rows(200)
        .with_max_bytes(max_bytes);
    let mut stream = s
        .execute(
            Statement::sql("SELECT * FROM numbers(1000000)"),
            opts(budget),
        )
        .await
        .unwrap();
    let (rows, items, err) = drain(&mut stream).await;
    assert!(err.is_none());
    assert!(rows > 0 && rows < 1_000_000);
    let Some(ResultItem::Truncated(t)) = items.last() else {
        panic!("expected Truncated");
    };
    assert_eq!(t.reason, TruncationReason::MaxBytes);
    assert!(t.bytes_delivered <= max_bytes, "{t:?}");
    assert_eq!(stream.bytes_delivered(), t.bytes_delivered);
    assert!(
        stats.rows_generated() < 10_000,
        "{}",
        stats.rows_generated()
    );
}

#[tokio::test]
async fn documents_respect_the_budget() {
    let driver = MockDriver::new();
    let mut s = connect(&driver).await;
    let budget = FetchBudget::default()
        .with_batch_rows(50)
        .with_max_rows(Some(120));
    let mut stream = s
        .execute(
            Statement::sql("SELECT * FROM documents(1000)"),
            opts(budget),
        )
        .await
        .unwrap();
    let (rows, items, _) = drain(&mut stream).await;
    assert_eq!(rows, 120);
    assert!(matches!(items.last(), Some(ResultItem::Truncated(_))));
}

#[tokio::test]
async fn dropping_a_stream_frees_the_session() {
    let driver = MockDriver::new();
    let mut s = connect(&driver).await;
    let mut stream = s
        .execute(
            Statement::sql("SELECT * FROM numbers(10000)"),
            ExecOptions::default(),
        )
        .await
        .unwrap();
    let _ = stream.next().await;
    let busy = s
        .execute(
            Statement::sql("SELECT * FROM numbers(1)"),
            ExecOptions::default(),
        )
        .await;
    assert_eq!(busy.err(), Some(DriverError::Busy));
    drop(stream);
    assert!(
        s.execute(
            Statement::sql("SELECT * FROM numbers(1)"),
            ExecOptions::default()
        )
        .await
        .is_ok()
    );
}

#[tokio::test(start_paused = true)]
async fn cancel_stops_the_stream_and_reaches_the_server() {
    let driver = MockDriver::new();
    let stats = driver.stats();
    let mut s = connect(&driver).await;
    let cancel = CancelToken::new();
    let options = ExecOptions::default()
        .with_cancel(cancel.clone())
        .with_budget(FetchBudget::default().with_batch_rows(10));
    let mut stream = s
        .execute(
            Statement::sql("SELECT * FROM slow_numbers(100000, 1000)"),
            options,
        )
        .await
        .unwrap();
    let consumer = tokio::spawn(async move {
        let r = drain(&mut stream).await;
        (r, stream)
    });
    tokio::time::sleep(Duration::from_millis(3500)).await;
    cancel.cancel();
    let ((rows, _, err), stream) = consumer.await.unwrap();
    assert_eq!(err, Some(DriverError::Cancelled));
    assert_eq!(rows, 30);
    assert_eq!(stats.server_cancels(), 1);
    assert!(stream.is_finished());
    drop(stream);
    // The session is usable again.
    let mut again = s
        .execute(
            Statement::sql("SELECT * FROM numbers(3)"),
            ExecOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(drain(&mut again).await.0, 3);
}

#[tokio::test]
async fn cancelled_before_start_yields_cancelled() {
    let driver = MockDriver::new();
    let mut s = connect(&driver).await;
    let options = ExecOptions::default();
    options.cancel.cancel();
    let mut stream = s
        .execute(Statement::sql("SELECT * FROM numbers(10)"), options)
        .await
        .unwrap();
    assert_eq!(
        stream.next().await.unwrap().err(),
        Some(DriverError::Cancelled)
    );
    assert!(stream.next().await.is_none());
}

#[tokio::test(start_paused = true)]
async fn timeout_cancels_server_side() {
    let driver = MockDriver::new();
    let stats = driver.stats();
    let mut s = connect(&driver).await;
    let options = ExecOptions::default().with_timeout(Some(Duration::from_secs(5)));
    let mut stream = s
        .execute(
            Statement::sql("SELECT * FROM slow_numbers(10000, 2000)"),
            options,
        )
        .await
        .unwrap();
    let (rows, _, err) = drain(&mut stream).await;
    assert_eq!(err, Some(DriverError::Timeout(Duration::from_secs(5))));
    // Batches arrive at t = 2 s and 4 s; the third (6 s) is past the deadline.
    assert_eq!(rows, 400);
    assert_eq!(stats.server_cancels(), 1);
    assert!(stream.next().await.is_none());
}

#[tokio::test]
async fn read_only_session_refuses_writes() {
    let driver = MockDriver::new();
    let stats = driver.stats();
    let mut s = connect_read_only(&driver).await;
    assert!(s.is_read_only());
    for sql in [
        "INSERT INTO t VALUES (1)",
        "update t set a = 1 where id = 1",
        "delete from t",
        "drop table t",
        "create table x (a int)",
        "SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE",
        "with d as (delete from t returning *) select * from d",
        "vacuum",
    ] {
        let r = s.execute(Statement::sql(sql), ExecOptions::default()).await;
        assert!(matches!(r, Err(DriverError::ReadOnly(_))), "{sql}: {r:?}");
    }
    // Reads still work.
    let mut stream = s
        .execute(
            Statement::sql("SELECT * FROM numbers(5)"),
            ExecOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(drain(&mut stream).await.0, 5);
    drop(stream);
    // Switching read-only off re-allows writes.
    s.set_read_only(false).await.unwrap();
    let mut stream = s
        .execute(
            Statement::sql("INSERT INTO t VALUES (1)"),
            ExecOptions::default(),
        )
        .await
        .unwrap();
    let (_, items, _) = drain(&mut stream).await;
    assert!(matches!(items[..], [ResultItem::UpdateCount(Some(1))]));
    assert_eq!(stats.server_rejected_writes(), 0);
}

#[tokio::test]
async fn read_only_applies_before_init_statements() {
    let driver = MockDriver::new();
    let mut p = params();
    p.read_only = true;
    p.init_statements = vec!["SET default_transaction_read_only = off".into()];
    let r = driver.connect(p).await;
    assert!(matches!(r, Err(DriverError::ReadOnly(_))));
}

#[tokio::test]
async fn client_side_only_engines_are_declared_as_such() {
    let mut caps = ninesql_driver_api::mock::full_capabilities();
    caps.read_only = ReadOnlyMechanism::ClientSideOnly;
    let driver = MockDriver::new().with_capabilities(caps);
    let mut s = connect_read_only(&driver).await;
    assert!(!s.capabilities().supports(Feature::EngineSideReadOnly));
    let r = s
        .execute(
            Statement::sql("insert into t values (1)"),
            ExecOptions::default(),
        )
        .await;
    assert!(matches!(r, Err(DriverError::ReadOnly(_))));
}

#[tokio::test]
async fn capabilities_override_hides_features() {
    let driver = MockDriver::new().with_capabilities(Capabilities::minimal(MOCK_ENGINE));
    let caps = driver.capabilities(&MOCK_ENGINE, None).unwrap();
    assert!(!caps.supports(Feature::Transactions));
    let mut s = connect(&driver).await;
    assert_eq!(
        s.set_auto_commit(false).await,
        Err(DriverError::Unsupported(Feature::Transactions))
    );
    assert_eq!(
        s.begin().await,
        Err(DriverError::Unsupported(Feature::Transactions))
    );
    let r = s
        .execute(
            Statement::sql("select * from numbers(1)")
                .with_params(vec![ninesql_driver_api::Param::Int(1)]),
            ExecOptions::default(),
        )
        .await;
    assert_eq!(r.err(), Some(DriverError::Unsupported(Feature::Parameters)));
    assert!(
        driver
            .capabilities(&ninesql_driver_api::EngineId::POSTGRESQL, None)
            .is_err()
    );
}

#[tokio::test]
async fn transactions_track_pending_work() {
    let driver = MockDriver::new();
    let mut s = connect(&driver).await;
    s.set_auto_commit(false).await.unwrap();
    assert!(!s.in_transaction());
    drop(
        s.execute(
            Statement::sql("insert into t values (1)"),
            ExecOptions::default(),
        )
        .await
        .unwrap(),
    );
    assert!(s.in_transaction());
    s.rollback().await.unwrap();
    assert!(!s.in_transaction());
}

#[tokio::test]
async fn multiple_result_sets_and_notices() {
    let driver = MockDriver::new();
    let mut s = connect(&driver).await;
    let mut stream = s
        .execute(
            Statement::sql("SELECT * FROM multi()"),
            ExecOptions::default(),
        )
        .await
        .unwrap();
    let (rows, items, err) = drain(&mut stream).await;
    assert!(err.is_none());
    assert_eq!(rows, 3);
    let headers: Vec<u32> = items
        .iter()
        .filter_map(|i| match i {
            ResultItem::ResultSet(h) => Some(h.index),
            _ => None,
        })
        .collect();
    assert_eq!(headers, vec![0, 1]);
    assert!(items.iter().any(|i| matches!(i, ResultItem::Notice(_))));
}

#[tokio::test]
async fn query_errors_are_typed() {
    let driver = MockDriver::new();
    let mut s = connect(&driver).await;
    let mut stream = s
        .execute(Statement::sql("SELECT error()"), ExecOptions::default())
        .await
        .unwrap();
    let (_, _, err) = drain(&mut stream).await;
    assert!(matches!(err, Some(DriverError::Query { code: Some(c), .. }) if c == "XX000"));
}

#[tokio::test]
async fn auth_errors_never_echo_the_password() {
    let driver = MockDriver::new().with_password(Secret::new("right-pw".into()));
    let mut p = params();
    p.auth = Auth::password("nine", Some(Secret::new("Wrong-Pa55word".into())));
    let err = driver.connect(p).await.err().unwrap();
    assert!(matches!(err, DriverError::Auth(_)));
    assert!(!err.to_string().contains("Wrong-Pa55word"));
    assert!(!format!("{err:?}").contains("Wrong-Pa55word"));

    let mut p = params();
    p.auth = Auth::password("nine", Some(Secret::new("right-pw".into())));
    assert!(driver.connect(p).await.is_ok());
    assert_eq!(driver.stats().connects(), 1);
}

#[tokio::test]
async fn metadata_is_lazy_and_paged() {
    let driver = MockDriver::new();
    let s = connect(&driver).await;
    let md = s.metadata();
    assert_eq!(md.child_kinds(None), vec![ObjectKind::Catalog]);
    let cat = md
        .list(
            &ObjectPath::root(),
            ObjectKind::Catalog,
            PageRequest::first(),
        )
        .await
        .unwrap();
    let schema_parent = cat.items[0].path.clone();
    let schemas = md
        .list(&schema_parent, ObjectKind::Schema, PageRequest::first())
        .await
        .unwrap();
    let public = schemas.items[0].path.clone();

    let mut seen = 0;
    let mut cursor = None;
    loop {
        let page = md
            .list(
                &public,
                ObjectKind::Table,
                PageRequest::first().with_cursor(cursor).with_limit(20),
            )
            .await
            .unwrap();
        seen += page.items.len();
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(seen, MOCK_TABLE_COUNT);

    let table = public.child(ObjectKind::Table, "t007");
    let cols = md
        .list(&table, ObjectKind::Column, PageRequest::first())
        .await
        .unwrap();
    assert_eq!(cols.items.len(), 2);
    assert!(
        md.ddl(&table)
            .await
            .unwrap()
            .starts_with("CREATE TABLE \"t007\"")
    );
    assert!(matches!(
        md.ddl(&table.child(ObjectKind::Column, "id")).await,
        Err(DriverError::Unsupported(_))
    ));
}

#[tokio::test]
async fn into_stream_adapter() {
    use futures_util::StreamExt;
    let driver = MockDriver::new();
    let mut s = connect(&driver).await;
    let stream = s
        .execute(
            Statement::sql("SELECT * FROM numbers(450)"),
            ExecOptions::default(),
        )
        .await
        .unwrap();
    let items: Vec<_> = stream.into_stream().collect().await;
    assert_eq!(items.len(), 4);
}

#[tokio::test]
async fn sessions_are_object_safe_and_send() {
    fn assert_send<T: Send>(_: &T) {}
    let driver: std::sync::Arc<dyn Driver> = std::sync::Arc::new(MockDriver::new());
    let s = driver.connect(params()).await.unwrap();
    assert_send(&s);
    let handle = s.cancel_handle();
    tokio::spawn(async move { handle.cancel().await })
        .await
        .unwrap()
        .unwrap();
    s.close().await.unwrap();
}

// ---------------------------------------------------------------------------------------------
// P0-B security review fixes (H1–H4, L7)

#[tokio::test]
async fn conformance_suite_passes_on_the_mock() {
    let driver = MockDriver::new();
    conformance::assert_reserved_properties_rejected(&driver, &params()).await;
    let mut writes = conformance::WRITES.to_vec();
    writes.extend(conformance::POSTGRES_ACCESS_MODE_WRITES);
    writes.extend(conformance::MYSQL_ACCESS_MODE_WRITES);
    writes.extend(conformance::SQLITE_ACCESS_MODE_WRITES);
    conformance::assert_read_only_refuses_writes(&driver, params(), &writes).await;
    conformance::assert_multiple_statements_refused(&driver, params()).await;
    conformance::assert_access_mode_change_detected(
        &driver,
        params(),
        "SELECT mock_lift_read_only()",
    )
    .await;
}

#[tokio::test]
async fn harmless_properties_are_accepted() {
    let driver = MockDriver::new();
    let mut p = params();
    p.properties
        .insert("connectTimeout".into(), PropertyValue::Plain("5".into()));
    assert!(driver.connect(p).await.is_ok());
}

#[tokio::test]
async fn multi_statement_text_is_refused_before_classification() {
    let driver = MockDriver::new();
    let mut s = connect_read_only(&driver).await;
    let r = s
        .execute(
            Statement::sql("SELECT * FROM numbers(1); SELECT * FROM numbers(2)"),
            ExecOptions::default(),
        )
        .await;
    assert_eq!(r.err(), Some(DriverError::MultipleStatements(2)));
}

#[tokio::test]
async fn engine_rejects_a_write_the_classifier_missed() {
    let driver = MockDriver::new();
    let stats = driver.stats();
    let mut s = connect_read_only(&driver).await;
    let r = s
        .execute(
            Statement::sql("SELECT mock_write()"),
            ExecOptions::default(),
        )
        .await;
    assert!(matches!(r, Err(DriverError::ReadOnly(_))), "{r:?}");
    assert_eq!(stats.server_rejected_writes(), 1);
    // Outside read-only mode the same statement writes.
    let mut rw = connect(&driver).await;
    let mut stream = rw
        .execute(
            Statement::sql("SELECT mock_write()"),
            ExecOptions::default(),
        )
        .await
        .unwrap();
    let (_, items, _) = drain(&mut stream).await;
    assert!(matches!(items[..], [ResultItem::UpdateCount(Some(1))]));
}

#[tokio::test]
async fn lifted_access_mode_closes_the_session() {
    let driver = MockDriver::new();
    let mut s = connect_read_only(&driver).await;
    assert!(s.capabilities().read_only.is_liftable());
    let r = s
        .execute(
            Statement::sql("SELECT mock_lift_read_only()"),
            ExecOptions::default(),
        )
        .await;
    assert!(matches!(r, Err(DriverError::ReadOnly(_))), "{r:?}");
    let r = s
        .execute(
            Statement::sql("SELECT * FROM numbers(1)"),
            ExecOptions::default(),
        )
        .await;
    assert!(matches!(r, Err(DriverError::Closed(_))), "{r:?}");
    assert!(s.ping().await.is_err());
}

#[tokio::test]
async fn batches_end_at_the_batch_byte_budget() {
    let driver = MockDriver::new();
    let mut s = connect(&driver).await;
    // 1000-byte values, 10 kB per batch: about 10 rows per batch whatever batch_rows says.
    let budget = FetchBudget::default()
        .with_batch_rows(u32::MAX)
        .with_max_batch_bytes(10_000);
    let mut stream = s
        .execute(
            Statement::sql("SELECT * FROM wide(100, 1000)"),
            opts(budget),
        )
        .await
        .unwrap();
    let (rows, items, err) = drain(&mut stream).await;
    assert!(err.is_none());
    assert_eq!(rows, 100);
    for item in &items {
        if let ResultItem::Rows(b) = item {
            assert!(b.num_rows() <= 10, "{} rows", b.num_rows());
        }
    }
}

#[tokio::test]
async fn huge_rows_are_bounded_by_batch_rows_clamp() {
    let driver = MockDriver::new();
    let stats = driver.stats();
    let mut s = connect(&driver).await;
    let budget = FetchBudget::default().with_batch_rows(u32::MAX);
    let mut stream = s
        .execute(
            Statement::sql("SELECT * FROM numbers(1000000)"),
            opts(budget),
        )
        .await
        .unwrap();
    let _header = stream.next().await;
    let first = stream.next().await.unwrap().unwrap();
    let ResultItem::Rows(b) = first else {
        panic!("expected rows");
    };
    assert!(b.num_rows() <= FetchBudget::MAX_BATCH_ROWS as usize);
    assert!(stats.rows_generated() <= u64::from(FetchBudget::MAX_BATCH_ROWS));
}

#[tokio::test]
async fn oversized_values_are_cut_and_flagged() {
    let driver = MockDriver::new();
    let mut s = connect(&driver).await;
    let budget = FetchBudget::default().with_max_value_bytes(100);
    let mut stream = s
        .execute(Statement::sql("SELECT * FROM wide(3, 5000)"), opts(budget))
        .await
        .unwrap();
    let (_, items, _) = drain(&mut stream).await;
    let batch = items
        .iter()
        .find_map(|i| match i {
            ResultItem::Rows(b) => Some(b.clone()),
            _ => None,
        })
        .unwrap();
    let field = batch.schema().field_with_name("label").unwrap().clone();
    assert_eq!(
        field
            .metadata()
            .get(TRUNCATED_VALUES_METADATA_KEY)
            .map(String::as_str),
        Some("true")
    );
    let labels = batch
        .column(1)
        .as_any()
        .downcast_ref::<ninesql_driver_api::arrow_array::StringArray>()
        .unwrap();
    assert!(labels.iter().all(|v| v.unwrap().len() <= 100));
}

#[tokio::test]
async fn notices_count_against_the_budget() {
    let driver = MockDriver::new();
    let mut s = connect(&driver).await;
    let budget = FetchBudget::default().with_max_bytes(1000);
    let mut stream = s
        .execute(
            Statement::sql("SELECT * FROM notices(1000000)"),
            opts(budget),
        )
        .await
        .unwrap();
    let (_, items, err) = drain(&mut stream).await;
    assert!(err.is_none());
    let notices = items
        .iter()
        .filter(|i| matches!(i, ResultItem::Notice(_)))
        .count();
    assert!(notices < 20, "{notices} notices delivered");
    assert!(matches!(
        items.last(),
        Some(ResultItem::Truncated(t)) if t.reason == TruncationReason::MaxBytes
    ));
    assert!(stream.bytes_delivered() <= 1000);
}

// ---------------------------------------------------------------------------------------------
// Phase 0 follow-ups landed before the first P1 driver (M6, L1–L5, F1–F7, conformance helpers)

#[tokio::test]
async fn new_conformance_helpers_pass_on_the_mock() {
    let driver = MockDriver::new();
    let rows = conformance::assert_stale_cancel_ignored(
        &driver,
        params(),
        "SELECT * FROM numbers(5)",
        "SELECT * FROM slow_numbers(50, 5)",
    )
    .await;
    assert_eq!(rows, 50);
    assert_eq!(
        driver.stats().server_cancels(),
        0,
        "a stale cancel reached the server"
    );

    conformance::assert_identifier_quoting(
        &driver,
        params(),
        conformance::ALIAS_TEMPLATE,
        conformance::IDENTIFIER_SAMPLES,
    )
    .await;

    let budget = FetchBudget::default()
        .with_max_value_bytes(300)
        .with_max_row_bytes(500)
        .with_max_batch_bytes(4_000);
    let report = conformance::assert_batches_within_budget(
        &driver,
        params(),
        "SELECT * FROM wide(200, 5000)",
        budget,
    )
    .await;
    assert_eq!(report.rows, 200);
    assert!(report.batches > 1 && report.truncated_values, "{report:?}");
    let report = conformance::assert_batches_within_budget(
        &driver,
        params(),
        "SELECT * FROM wide_documents(100, 5000)",
        budget,
    )
    .await;
    assert_eq!(report.rows, 100);
    assert!(report.batches > 1 && report.truncated_values, "{report:?}");

    let tls_params = || {
        let mut p = ConnectParams::new(
            MOCK_ENGINE,
            Endpoint::tcp(ninesql_driver_api::mock::MOCK_SELF_SIGNED_HOST, 5432),
        );
        p.auth = Auth::password("nine", Some(Secret::new("self-signed-pw".into())));
        p
    };
    conformance::assert_verify_full_rejects_untrusted_certificate(
        &driver,
        &tls_params,
        std::path::Path::new("/dev-certs/mock-ca.pem"),
        "wrong.example.test",
    )
    .await;
}

#[test]
fn writes_lists_are_per_dialect() {
    use ninesql_driver_api::EngineId;
    let pg = conformance::writes_for(&EngineId::POSTGRESQL);
    let my = conformance::writes_for(&EngineId::MARIADB);
    let lite = conformance::writes_for(&EngineId::SQLITE);
    assert!(pg.iter().any(|w| w.contains("set_config")));
    assert!(!my.iter().any(|w| w.contains("set_config")));
    assert!(my.iter().any(|w| w.contains("@@session")));
    assert!(lite.iter().any(|w| w.contains("query_only")));
    assert_eq!(
        conformance::writes_for(&EngineId::CASSANDRA),
        conformance::WRITES.to_vec()
    );
    // Every list is refused by the classifier of its own dialect family.
    use ninesql_driver_api::dialect::classify_with_rules;
    use ninesql_driver_api::{LexicalRules, StatementClass};
    for (list, rules) in [
        (pg, LexicalRules::POSTGRES),
        (my, LexicalRules::MYSQL),
        (lite, LexicalRules::SQLITE),
    ] {
        for w in list {
            assert!(classify_with_rules(w, rules).is_write(), "{w}");
            assert!(
                classify_with_rules(w, rules) >= StatementClass::Write,
                "{w}"
            );
        }
    }
}

/// M6: a cancelled statement keeps the session busy until the server confirmed its end.
#[tokio::test(start_paused = true)]
async fn session_stays_busy_until_the_cancelled_statement_ends() {
    let driver = MockDriver::new();
    let stats = driver.stats();
    let mut s = connect(&driver).await;
    let cancel = CancelToken::new();
    let options = ExecOptions::default()
        .with_cancel(cancel.clone())
        .with_budget(FetchBudget::default().with_batch_rows(10));
    let mut stream = s
        .execute(
            Statement::sql("SELECT * FROM slow_numbers(100000, 1000)"),
            options,
        )
        .await
        .unwrap();
    assert!(stream.execution_id().is_some());
    let consumer = tokio::spawn(async move {
        let r = drain(&mut stream).await;
        (r, stream)
    });
    tokio::time::sleep(Duration::from_millis(3500)).await;
    cancel.cancel();
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    // The cancel reached the server, but the server is still finishing the batch it started:
    // no new statement may start (it could be hit by the cancel).
    assert_eq!(stats.server_cancels(), 1);
    let busy = s
        .execute(
            Statement::sql("SELECT * FROM numbers(1)"),
            ExecOptions::default(),
        )
        .await;
    assert_eq!(busy.err(), Some(DriverError::Busy));
    let ((rows, _, err), stream) = consumer.await.unwrap();
    assert_eq!(err, Some(DriverError::Cancelled));
    assert_eq!(rows, 30, "nothing from the drain is delivered");
    assert!(stream.is_finished());
    // Confirmed: the session is free even before the stream object is dropped.
    let mut again = s
        .execute(
            Statement::sql("SELECT * FROM numbers(3)"),
            ExecOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(drain(&mut again).await.0, 3);
    drop(stream);
    assert_eq!(stats.server_cancels(), 1);
}

/// M6: the stream's own handle and the session's handle are scoped to their execution.
#[tokio::test]
async fn cancel_handles_are_execution_scoped() {
    let driver = MockDriver::new();
    let stats = driver.stats();
    let mut s = connect(&driver).await;
    // Idle: the session handle does nothing.
    s.cancel().await.unwrap();
    assert_eq!(stats.server_cancels(), 0);
    let mut a = s
        .execute(
            Statement::sql("SELECT * FROM numbers(10)"),
            ExecOptions::default(),
        )
        .await
        .unwrap();
    let a_id = a.execution_id().unwrap();
    let handle = a.cancel_handle();
    drain(&mut a).await;
    drop(a);
    let mut b = s
        .execute(
            Statement::sql("SELECT * FROM numbers(1000)"),
            ExecOptions::default(),
        )
        .await
        .unwrap();
    assert!(b.execution_id().unwrap() > a_id);
    handle.cancel().await.unwrap();
    let (rows, _, err) = drain(&mut b).await;
    assert!(err.is_none(), "{err:?}");
    assert_eq!(rows, 1000);
    assert_eq!(stats.server_cancels(), 0);
}

/// L2: the timeout cancels the server cursor at the deadline even if nobody polls.
#[tokio::test(start_paused = true)]
async fn timeout_cancels_the_server_cursor_while_not_polled() {
    let driver = MockDriver::new();
    let stats = driver.stats();
    let mut s = connect(&driver).await;
    let options = ExecOptions::default()
        .with_timeout(Some(Duration::from_secs(5)))
        .with_budget(FetchBudget::default().with_batch_rows(10));
    let mut stream = s
        .execute(
            Statement::sql("SELECT * FROM slow_numbers(100000, 1000)"),
            options,
        )
        .await
        .unwrap();
    let _header = stream.next().await.unwrap().unwrap();
    // The grid shows its first page and stops polling.
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert_eq!(stats.server_cancels(), 0);
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(
        stats.server_cancels(),
        1,
        "watchdog did not cancel at the deadline"
    );
    // The next poll reports the timeout without a second cancel.
    assert_eq!(
        stream.next().await.unwrap().err(),
        Some(DriverError::Timeout(Duration::from_secs(5)))
    );
    assert!(stream.next().await.is_none());
    assert_eq!(stats.server_cancels(), 1);
    // Rows generated: only what the consumer pulled before the pause plus the drained batch.
    assert!(stats.rows_generated() <= 10, "{}", stats.rows_generated());
}

#[tokio::test(start_paused = true)]
async fn watchdog_stops_with_the_stream() {
    let driver = MockDriver::new();
    let stats = driver.stats();
    let mut s = connect(&driver).await;
    let options = ExecOptions::default().with_timeout(Some(Duration::from_secs(5)));
    let mut stream = s
        .execute(Statement::sql("SELECT * FROM numbers(10)"), options)
        .await
        .unwrap();
    drain(&mut stream).await;
    drop(stream);
    tokio::time::sleep(Duration::from_secs(10)).await;
    assert_eq!(stats.server_cancels(), 0, "watchdog fired after the end");
}

fn label_batch(rows: usize, value_len: usize) -> ninesql_driver_api::arrow_array::RecordBatch {
    use ninesql_driver_api::arrow_array::{RecordBatch, StringArray};
    use ninesql_driver_api::arrow_schema::{DataType, Field, Schema};
    let schema = std::sync::Arc::new(Schema::new(vec![Field::new(
        "label",
        DataType::Utf8,
        false,
    )]));
    let values: Vec<String> = (0..rows).map(|_| "z".repeat(value_len)).collect();
    RecordBatch::try_new(schema, vec![std::sync::Arc::new(StringArray::from(values))]).unwrap()
}

fn header_of(batch: &ninesql_driver_api::arrow_array::RecordBatch) -> ResultItem {
    ResultItem::ResultSet(ninesql_driver_api::ResultSetHeader::tabular(
        0,
        batch.schema(),
    ))
}

/// L3: a cut batch is copied, so the retained memory is what is counted.
#[tokio::test]
async fn cut_batches_are_compacted_and_counted_honestly() {
    let batch = label_batch(1000, 1000);
    let full = batch.get_array_memory_size() as u64;
    let max_bytes = 100_000;
    let opts = ExecOptions::default().with_budget(FetchBudget::default().with_max_bytes(max_bytes));
    let mut stream = ResultStream::from_items(
        vec![Ok(header_of(&batch)), Ok(ResultItem::Rows(batch))],
        &opts,
    );
    let (rows, items, err) = drain(&mut stream).await;
    assert!(err.is_none());
    assert!(rows > 0 && rows < 1000, "{rows}");
    let retained: u64 = items
        .iter()
        .filter_map(|i| match i {
            ResultItem::Rows(b) => Some(b.get_array_memory_size() as u64),
            _ => None,
        })
        .sum();
    assert!(
        retained < full / 5,
        "the slice keeps the whole buffer: {retained} of {full}"
    );
    assert!(stream.bytes_delivered() <= max_bytes);
    assert!(
        matches!(items.last(), Some(ResultItem::Truncated(t)) if t.reason == TruncationReason::MaxBytes)
    );
}

/// L3: a `Truncated` sent by the driver ends the stream.
#[tokio::test]
async fn driver_sent_truncation_is_terminal() {
    use ninesql_driver_api::Truncation;
    let batch = label_batch(3, 4);
    let items = vec![
        Ok(header_of(&batch)),
        Ok(ResultItem::Rows(batch.clone())),
        Ok(ResultItem::Truncated(Truncation::new(
            TruncationReason::MaxRows,
            3,
            100,
        ))),
        Ok(ResultItem::Rows(batch)),
        Ok(ResultItem::UpdateCount(Some(1))),
    ];
    let mut stream = ResultStream::from_items(items, &ExecOptions::default());
    let (rows, items, err) = drain(&mut stream).await;
    assert!(err.is_none());
    assert_eq!(rows, 3);
    assert!(matches!(items.last(), Some(ResultItem::Truncated(_))));
    assert!(stream.is_finished());
}

/// L3: batches are checked against their header, up to field metadata.
#[tokio::test]
async fn batches_must_match_their_header() {
    use ninesql_driver_api::arrow_array::{Int64Array, RecordBatch};
    use ninesql_driver_api::arrow_schema::{DataType, Field, Schema};
    let batch = label_batch(2, 4);
    let other = RecordBatch::try_new(
        std::sync::Arc::new(Schema::new(vec![Field::new(
            "label",
            DataType::Int64,
            false,
        )])),
        vec![std::sync::Arc::new(Int64Array::from(vec![1, 2]))],
    )
    .unwrap();
    let flagged = RecordBatch::try_new(
        std::sync::Arc::new(Schema::new(vec![
            Field::new("label", DataType::Utf8, false)
                .with_metadata([(TRUNCATED_VALUES_METADATA_KEY.to_owned(), "true".to_owned())]),
        ])),
        batch.columns().to_vec(),
    )
    .unwrap();
    let docs_header = ResultItem::ResultSet(ninesql_driver_api::ResultSetHeader::documents(0));
    let doc =
        ninesql_driver_api::Document::new(vec![("a".into(), ninesql_driver_api::DocValue::Int(1))]);
    for (case, items, ok) in [
        (
            "metadata only",
            vec![header_of(&batch), ResultItem::Rows(flagged)],
            true,
        ),
        (
            "other type",
            vec![header_of(&batch), ResultItem::Rows(other)],
            false,
        ),
        ("no header", vec![ResultItem::Rows(batch.clone())], false),
        (
            "rows in documents",
            vec![docs_header.clone(), ResultItem::Rows(batch.clone())],
            false,
        ),
        (
            "documents in tabular",
            vec![header_of(&batch), ResultItem::Documents(vec![doc.clone()])],
            false,
        ),
        (
            "documents",
            vec![docs_header, ResultItem::Documents(vec![doc])],
            true,
        ),
    ] {
        let mut stream =
            ResultStream::from_items(items.into_iter().map(Ok).collect(), &ExecOptions::default());
        let (_, _, err) = drain(&mut stream).await;
        if ok {
            assert!(err.is_none(), "{case}: {err:?}");
        } else {
            assert!(
                matches!(err, Some(DriverError::Protocol(_))),
                "{case}: {err:?}"
            );
            assert!(stream.is_finished());
        }
    }
}

/// F5: result-set headers count against the budget.
#[tokio::test]
async fn headers_count_against_the_budget() {
    let items: Vec<_> = (0..100_000u32)
        .map(|i| {
            Ok(ResultItem::ResultSet(
                ninesql_driver_api::ResultSetHeader::documents(i),
            ))
        })
        .collect();
    let opts = ExecOptions::default().with_budget(FetchBudget::default().with_max_bytes(10_000));
    let mut stream = ResultStream::from_items(items, &opts);
    let (_, items, err) = drain(&mut stream).await;
    assert!(err.is_none());
    assert!(items.len() < 200, "{} headers delivered", items.len());
    assert!(
        matches!(items.last(), Some(ResultItem::Truncated(t)) if t.reason == TruncationReason::MaxBytes)
    );
    assert!(stream.bytes_delivered() <= 10_000);
}

/// F4: cut document values are flagged and make the document non-editable.
#[tokio::test]
async fn truncated_document_values_are_flagged() {
    let driver = MockDriver::new();
    let mut s = connect(&driver).await;
    let budget = FetchBudget::default().with_max_value_bytes(100);
    let mut stream = s
        .execute(
            Statement::sql("SELECT * FROM wide_documents(2, 5000)"),
            opts(budget),
        )
        .await
        .unwrap();
    let (_, items, _) = drain(&mut stream).await;
    let docs: Vec<_> = items
        .iter()
        .filter_map(|i| match i {
            ResultItem::Documents(d) => Some(d.clone()),
            _ => None,
        })
        .flatten()
        .collect();
    assert_eq!(docs.len(), 2);
    for d in &docs {
        assert!(d.has_truncated_values());
        let payload = &d.fields.iter().find(|(k, _)| k == "payload").unwrap().1;
        let ninesql_driver_api::DocValue::Truncated { kept, original_len } = payload else {
            panic!("payload not flagged: {payload:?}");
        };
        assert_eq!(*original_len, Some(5000));
        assert!(matches!(kept.as_ref(), ninesql_driver_api::DocValue::String(v) if v.len() == 100));
    }
}

/// Integration review: a driver's extra reserved keys are exposed and enforced.
#[tokio::test]
async fn extra_reserved_properties_are_exposed_and_enforced() {
    use ninesql_driver_api::mock::MOCK_RESERVED_PROPERTY;
    let driver = MockDriver::new();
    let caps = driver.capabilities(&MOCK_ENGINE, None).unwrap();
    assert!(
        caps.extra_reserved_properties
            .contains(MOCK_RESERVED_PROPERTY)
    );
    assert!(caps.is_reserved_property("Mock-Backdoor"));
    assert!(!ninesql_driver_api::is_reserved_property("mock_backdoor"));
    assert!(!caps.retains_secret_after_connect);
    let mut p = params();
    p.properties.insert(
        "mockBackdoor".into(),
        PropertyValue::Plain("open-sesame".into()),
    );
    let err = driver.connect(p).await.err().unwrap();
    assert!(matches!(err, DriverError::InvalidParams(_)));
    assert!(!err.to_string().contains("open-sesame"));
}

// ---------------------------------------------------------------------------------------------
// Security review of the follow-ups (M-1, M-3, L-2, L-4, L-5, L-8)

struct SlowCancel {
    started: std::sync::atomic::AtomicU32,
    done: std::sync::atomic::AtomicU32,
}

impl SlowCancel {
    fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            started: std::sync::atomic::AtomicU32::new(0),
            done: std::sync::atomic::AtomicU32::new(0),
        })
    }
    fn counts(&self) -> (u32, u32) {
        use std::sync::atomic::Ordering::SeqCst;
        (self.started.load(SeqCst), self.done.load(SeqCst))
    }
}

#[async_trait::async_trait]
impl ninesql_driver_api::CancelHandle for SlowCancel {
    async fn cancel(&self) -> ninesql_driver_api::DriverResult<()> {
        use std::sync::atomic::Ordering::SeqCst;
        self.started.fetch_add(1, SeqCst);
        tokio::time::sleep(Duration::from_millis(200)).await;
        self.done.fetch_add(1, SeqCst);
        Ok(())
    }
    fn confirms_end(&self) -> bool {
        true
    }
}

fn pending_raw() -> ninesql_driver_api::RawResultStream {
    Box::pin(futures_util::stream::pending())
}

/// M-1: a `next()` dropped while the cancel request is in flight does not lose the request,
/// and the retry does not send a second one.
#[tokio::test(start_paused = true)]
async fn dropped_abort_still_delivers_the_cancel_once() {
    let c = SlowCancel::new();
    let token = CancelToken::new();
    let opts = ExecOptions::default().with_cancel(token.clone());
    let mut s = ResultStream::new(pending_raw(), &opts, c.clone());
    token.cancel();
    assert!(
        tokio::time::timeout(Duration::from_millis(50), s.next())
            .await
            .is_err()
    );
    assert_eq!(c.counts(), (1, 0));
    let r = s.next().await;
    assert_eq!(r.unwrap().err(), Some(DriverError::Cancelled));
    assert_eq!(c.counts(), (1, 1), "the cancel was lost or sent twice");

    // Same on the timeout path, with the consumer polling: dropped once, then retried.
    let c = SlowCancel::new();
    let opts = ExecOptions::default().with_timeout(Some(Duration::from_secs(1)));
    let mut s = ResultStream::new(pending_raw(), &opts, c.clone());
    let _ = tokio::time::timeout(Duration::from_millis(1050), s.next()).await;
    let r = s.next().await;
    assert_eq!(
        r.unwrap().err(),
        Some(DriverError::Timeout(Duration::from_secs(1)))
    );
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(c.counts(), (1, 1));
}

/// M-1: the watchdog and the poll path share one cancel request.
#[tokio::test(start_paused = true)]
async fn watchdog_and_poll_send_one_cancel() {
    let c = SlowCancel::new();
    let opts = ExecOptions::default().with_timeout(Some(Duration::from_secs(1)));
    let mut s = ResultStream::new(pending_raw(), &opts, c.clone());
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(c.counts(), (1, 0), "watchdog request in flight");
    let r = s.next().await;
    assert_eq!(
        r.unwrap().err(),
        Some(DriverError::Timeout(Duration::from_secs(1)))
    );
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(c.counts(), (1, 1));
}

/// L-4: outside a runtime there is no watchdog (and no panic).
#[test]
fn stream_without_runtime_has_no_watchdog() {
    let c = SlowCancel::new();
    let opts = ExecOptions::default().with_timeout(Some(Duration::from_secs(1)));
    drop(ResultStream::new(pending_raw(), &opts, c.clone()));
    assert_eq!(c.counts(), (0, 0));
}

/// M-6 / reviewer case: a late confirmation for an old execution never frees the next one.
#[test]
fn late_confirmation_does_not_free_the_next_execution() {
    let t = ninesql_driver_api::ExecutionTracker::new();
    let a = t.begin().unwrap();
    let a_id = a.id();
    drop(a);
    t.confirm_ended(a_id);
    let b = t.begin().unwrap();
    t.confirm_ended(a_id);
    assert!(t.is_busy());
    b.complete();
    assert!(!t.is_busy());
}

/// M-3: after the watchdog fired on a paused (idle) cursor, the next poll closes the cursor
/// instead of fetching more rows.
#[tokio::test(start_paused = true)]
async fn idle_cursor_is_closed_not_fetched_after_cancel() {
    let driver = MockDriver::new();
    let stats = driver.stats();
    let mut s = connect(&driver).await;
    let options = ExecOptions::default()
        .with_timeout(Some(Duration::from_secs(5)))
        .with_budget(FetchBudget::default().with_batch_rows(10));
    let mut stream = s
        .execute(
            Statement::sql("SELECT * FROM slow_numbers(100000, 100)"),
            options,
        )
        .await
        .unwrap();
    let _header = stream.next().await.unwrap().unwrap();
    let _first = stream.next().await.unwrap().unwrap();
    let fetched = stats.rows_generated();
    tokio::time::sleep(Duration::from_secs(6)).await;
    assert_eq!(stats.server_cancels(), 1);
    assert_eq!(
        stream.next().await.unwrap().err(),
        Some(DriverError::Timeout(Duration::from_secs(5)))
    );
    assert_eq!(stats.cursors_closed_on_cancel(), 1);
    assert_eq!(
        stats.rows_generated(),
        fetched,
        "the drain fetched more rows"
    );
    drop(stream);
    assert!(
        s.execute(
            Statement::sql("SELECT * FROM numbers(1)"),
            ExecOptions::default()
        )
        .await
        .is_ok()
    );
}

/// L-5: empty batches cost budget.
#[tokio::test]
async fn empty_batches_count_against_the_budget() {
    let empty = label_batch(0, 0);
    let mut items = vec![Ok(header_of(&empty))];
    items.extend((0..100_000).map(|_| Ok(ResultItem::Rows(empty.clone()))));
    let opts = ExecOptions::default().with_budget(FetchBudget::default().with_max_bytes(10_000));
    let mut stream = ResultStream::from_items(items, &opts);
    let (_, items, err) = drain(&mut stream).await;
    assert!(err.is_none());
    assert!(items.len() < 200, "{} items delivered", items.len());
    assert!(matches!(items.last(), Some(ResultItem::Truncated(_))));
}

/// L-8: nesting is capped; the walks are iterative.
#[tokio::test]
async fn deep_documents_are_rejected() {
    use ninesql_driver_api::{DocValue, Document, MAX_DOCUMENT_DEPTH, ResultSetHeader};
    fn nested(depth: usize) -> Document {
        let mut v = DocValue::truncated(DocValue::Int(1), None);
        for _ in 1..depth {
            v = DocValue::Array(vec![v]);
        }
        Document::new(vec![("a".into(), v)])
    }
    let ok = nested(MAX_DOCUMENT_DEPTH);
    assert_eq!(ok.depth(), MAX_DOCUMENT_DEPTH);
    assert!(ok.has_truncated_values());
    let deep = nested(1000);
    assert_eq!(deep.depth(), 1000);
    assert!(deep.estimated_size() > 1000);
    for (doc, accepted) in [(ok, true), (deep, false)] {
        let items = vec![
            Ok(ResultItem::ResultSet(ResultSetHeader::documents(0))),
            Ok(ResultItem::Documents(vec![doc])),
        ];
        let mut stream = ResultStream::from_items(items, &ExecOptions::default());
        let (_, _, err) = drain(&mut stream).await;
        assert_eq!(err.is_none(), accepted, "{err:?}");
        if !accepted {
            assert!(matches!(err, Some(DriverError::Protocol(_))));
        }
    }
}

/// L-2: a driver whose server does not speak TLS cannot pass the TLS helper.
#[tokio::test]
#[should_panic(expected = "positive control")]
async fn tls_helper_needs_a_positive_control() {
    struct NoTls(MockDriver);
    #[async_trait::async_trait]
    impl Driver for NoTls {
        fn family(&self) -> &'static str {
            "no-tls"
        }
        fn engines(&self) -> &[ninesql_driver_api::EngineId] {
            self.0.engines()
        }
        fn capabilities(
            &self,
            e: &ninesql_driver_api::EngineId,
            v: Option<&str>,
        ) -> ninesql_driver_api::DriverResult<Capabilities> {
            self.0.capabilities(e, v)
        }
        fn dialect(
            &self,
            e: &ninesql_driver_api::EngineId,
        ) -> ninesql_driver_api::DriverResult<std::sync::Arc<dyn ninesql_driver_api::Dialect>>
        {
            self.0.dialect(e)
        }
        async fn connect(
            &self,
            _p: ConnectParams,
        ) -> ninesql_driver_api::DriverResult<Box<dyn Session>> {
            Err(DriverError::Tls("server does not support TLS".into()))
        }
    }
    let params = || ConnectParams::new(MOCK_ENGINE, Endpoint::tcp("plain.example.test", 5432));
    conformance::assert_verify_full_rejects_untrusted_certificate(
        &NoTls(MockDriver::new()),
        &params,
        std::path::Path::new("/dev-certs/mock-ca.pem"),
        "wrong.example.test",
    )
    .await;
}
