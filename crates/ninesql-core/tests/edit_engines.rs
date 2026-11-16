//! Edit service (P5-A core) and the P5-B transfer integration against real engines: the row key
//! from the metadata service, a save in one transaction with the row-count check, a stale edit
//! that rolls the whole save back, filtered and sorted table queries with bound values,
//! foreign-key navigation, and the "SQL INSERT" export replayed into a copy of the table.
//!
//! PostgreSQL, MySQL and MariaDB need `python3 dev/ready.py up core`; they are skipped (not
//! failed) when unreachable, unless `NINESQL_REQUIRE_ENGINES` lists them (AGENTS.md). SQLite
//! always runs.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stderr)] // Tests may panic (AGENTS.md).

mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use ninesql_core::driver_api::arrow_array::RecordBatch;
use ninesql_core::driver_api::{
    Driver, ForeignKeyInfo, ObjectDetails, ObjectKind, ObjectPath, Param, ResultItem,
};
use ninesql_core::edit::{referenced_rows_query, referencing_rows_query, row_values_by_name};
use ninesql_core::transfer::value::{CellValue, cell};
use ninesql_core::transfer::{ExportFormat, ExportOptions};
use ninesql_core::{
    Auth, BuiltQuery, ColumnFilter, Connection, ConnectionTypeId, ConnectionTypes, EditError,
    EditService, Endpoint, EngineId, ExecEvent, ExecuteRequest, ExecutionService, ExportEvent,
    ExportRequest, FilterOp, MetadataConfig, MetadataService, NodeKey, OpenOutcome, OpenRequest,
    PendingEdits, Preferences, SaveEvent, SaveJob, SaveOptions, SecretSlot, SecretSource,
    SessionHandle, SessionManager, SortDirection, SortKey, TableQuery, TlsMode, TransferService,
};
use ninesql_secrets::SecretString;
use ninesql_storage::{Paths, XdgDirs};
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
    manager: &SessionManager,
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

async fn exec_ok(h: &SessionHandle, sql: &str) {
    let events = run(h, sql).await;
    assert!(
        matches!(terminal(&events), ExecEvent::Finished { .. }),
        "{sql}: {:?}",
        terminal(&events)
    );
}

async fn query(h: &SessionHandle, text: &str, params: Vec<Param>) -> Vec<RecordBatch> {
    let events = collect(h.execute(ExecuteRequest::sql(text).with_params(params))).await;
    assert!(
        matches!(terminal(&events), ExecEvent::Finished { .. }),
        "{text}: {:?}",
        terminal(&events)
    );
    events
        .into_iter()
        .filter_map(|e| match e {
            ExecEvent::Item(ResultItem::Rows(b)) => Some(b),
            _ => None,
        })
        .collect()
}

async fn built(h: &SessionHandle, q: BuiltQuery) -> Vec<RecordBatch> {
    query(h, &q.text, q.params).await
}

fn row_count(batches: &[RecordBatch]) -> usize {
    batches.iter().map(RecordBatch::num_rows).sum()
}

fn int(b: &RecordBatch, col: usize, row: usize) -> i64 {
    match cell(b.column(col), row) {
        CellValue::Int(i) => i,
        CellValue::UInt(u) => i64::try_from(u).unwrap(),
        CellValue::Decimal(d) => d.parse().unwrap(),
        CellValue::Text(t) => t.parse().unwrap(),
        other => panic!("{other:?}"),
    }
}

fn text(b: &RecordBatch, col: usize, row: usize) -> Option<String> {
    match cell(b.column(col), row) {
        CellValue::Null => None,
        CellValue::Text(t) => Some(t.to_owned()),
        CellValue::Bytes(t) => Some(String::from_utf8(t.to_vec()).unwrap()),
        other => panic!("{other:?}"),
    }
}

/// The label of row `id`.
async fn label(h: &SessionHandle, table: &str, id: i64) -> Option<String> {
    let b = query(
        h,
        &format!("SELECT label FROM {table} WHERE id = {id}"),
        vec![],
    )
    .await;
    text(&b[0], 0, 0)
}

async fn end(mut job: SaveJob) -> SaveEvent {
    loop {
        match tokio::time::timeout(Duration::from_secs(60), job.next())
            .await
            .unwrap()
        {
            Some(
                e @ (SaveEvent::Done(_) | SaveEvent::Failed(_) | SaveEvent::NeedsConfirmation(_)),
            ) => {
                return e;
            }
            Some(_) => {}
            None => panic!("ended without a terminal event"),
        }
    }
}

struct Names {
    parent: String,
    child: String,
    copy: String,
}

fn names() -> Names {
    let pid = std::process::id();
    Names {
        parent: format!("p5a_parent_{pid}"),
        child: format!("p5a_child_{pid}"),
        copy: format!("p5a_copy_{pid}"),
    }
}

/// The whole scenario on an open session whose tables `names` exist in `schema`, filled with
/// rows (1, 'a'), (2, NULL), (3, 'c') and children (10 → 1), (11 → 1), (12 → 2).
#[allow(clippy::too_many_lines)] // One scenario, step by step.
async fn scenario(
    manager: &SessionManager,
    h: &SessionHandle,
    schema: &str,
    n: &Names,
    root: &Path,
) {
    let metadata = MetadataService::new(manager.clone(), MetadataConfig::default());
    let edit = EditService::new(
        manager.clone(),
        metadata.clone(),
        ExecutionService::new(manager.clone(), None, Preferences::default()),
    );
    let engine = h.info().engine.as_str().to_owned();
    let schema_path = ObjectPath::root().child(ObjectKind::Schema, schema);
    let parent_path = schema_path.child(ObjectKind::Table, &n.parent);
    let child_path = schema_path.child(ObjectKind::Table, &n.child);

    // The key, from the metadata service.
    let table = edit
        .describe_table(h, parent_path.clone())
        .await
        .unwrap()
        .unwrap_or_else(|e| panic!("{engine}: {e}"));
    assert_eq!(table.key.columns, vec!["id"], "{engine}");
    // The grid's query of the table: built by the core, so its result can be edited.
    let open_table = edit
        .table_query(
            h,
            &TableQuery::of_path(&parent_path)
                .with_columns(vec!["id".into(), "label".into()])
                .order(SortKey::new("id", SortDirection::Ascending)),
        )
        .unwrap();
    let source = open_table.source().clone();
    let select = open_table.text.clone();
    let before = built(h, open_table.clone()).await;
    let b = &before[0];
    assert_eq!(b.num_rows(), 3);
    // A statement typed by the user has no source of its own; paired with the table query's
    // source, a result whose columns differ from what that query selected is refused (here an
    // alias pointing `id` at another column).
    let aliased = query(
        h,
        &format!("SELECT label AS id FROM {} ORDER BY id", n.parent),
        vec![],
    )
    .await;
    let star = edit
        .table_query(h, &TableQuery::of_path(&parent_path))
        .unwrap();
    assert!(
        PendingEdits::new(table.clone(), star.source(), &aliased[0].schema()).is_err(),
        "{engine}: an aliased result was accepted"
    );

    // Edits: a label with quotes and comment markers, a NULL set, a delete, an insert.
    let mut p = PendingEdits::new(table.clone(), &source, &b.schema()).unwrap();
    let tricky = "it's \"x\"; -- /* \\ */";
    p.set_value(0, b, 0, 1, p.parse_input(1, tricky).unwrap())
        .unwrap();
    p.set_value(1, b, 1, 1, Param::Text("was null".into()))
        .unwrap();
    p.delete_row(2, b, 2).unwrap();
    let new = p.insert_row();
    p.set_new_value(new, 0, Some(p.parse_input(0, "4").unwrap()))
        .unwrap();
    p.set_new_value(new, 1, Some(Param::Text("d".into())))
        .unwrap();
    let plan = p
        .plan(&*h.dialect(), &h.info().capabilities.editing)
        .unwrap();
    assert_eq!(plan.batch.statements.len(), 4, "{engine}");
    let SaveEvent::Done(s) = end(edit.save(h, &p, SaveOptions::default())).await else {
        panic!("{engine}: not saved");
    };
    assert_eq!(s.rows_affected, 4, "{engine}");
    assert!(!h.transaction_state().in_transaction);
    assert!(!h.is_leased());
    assert_eq!(
        label(h, &n.parent, 1).await.as_deref(),
        Some(tricky),
        "{engine}"
    );
    assert_eq!(label(h, &n.parent, 2).await.as_deref(), Some("was null"));
    assert_eq!(label(h, &n.parent, 4).await.as_deref(), Some("d"));
    assert_eq!(
        row_count(
            &query(
                h,
                &format!("SELECT id FROM {} WHERE id = 3", n.parent),
                vec![]
            )
            .await
        ),
        0
    );

    // A stale edit (the row changed since it was read) fails its row-count check, and the
    // whole save rolls back, the fresh edit before it included.
    let fresh = query(h, &select, vec![]).await;
    let mut p = PendingEdits::new(table.clone(), &source, &fresh[0].schema()).unwrap();
    p.set_value(1, &fresh[0], 1, 1, Param::Text("fresh".into()))
        .unwrap();
    p.set_value(5, b, 0, 1, Param::Text("stale".into()))
        .unwrap();
    match end(edit.save(h, &p, SaveOptions::default())).await {
        SaveEvent::Failed(EditError::RowCount { index: 2, .. }) => {}
        other => panic!("{engine}: {other:?}"),
    }
    assert_eq!(
        label(h, &n.parent, 2).await.as_deref(),
        Some("was null"),
        "{engine}: not rolled back"
    );
    assert_eq!(label(h, &n.parent, 1).await.as_deref(), Some(tricky));
    assert!(!h.transaction_state().in_transaction);

    // Server-side filter and sort: values bound, names quoted.
    let q = TableQuery::of_path(&parent_path)
        .filter(ColumnFilter::new(
            "label",
            FilterOp::Contains,
            Some(Param::Text("'s \"x\"".into())),
        ))
        .order(SortKey::new("id", SortDirection::Descending));
    let rows = built(h, edit.table_query(h, &q).unwrap()).await;
    assert_eq!(row_count(&rows), 1, "{engine}");
    assert_eq!(int(&rows[0], 0, 0), 1);
    let q = TableQuery::of_path(&parent_path)
        .with_raw_where("id >= 2 AND label IS NOT NULL")
        .order(SortKey::new("id", SortDirection::Ascending));
    let rows = built(h, edit.table_query(h, &q).unwrap()).await;
    assert_eq!(row_count(&rows), 2, "{engine}");
    assert!(
        edit.table_query(
            h,
            &TableQuery::of_path(&parent_path).with_raw_where("1=1) UNION SELECT 1, 2 --")
        )
        .is_err()
    );

    // Foreign-key navigation.
    let fks = metadata
        .children(
            h.info().connection,
            NodeKey::new(child_path.clone(), ObjectKind::ForeignKey),
        )
        .await
        .unwrap();
    let fk: ForeignKeyInfo = fks
        .items
        .iter()
        .find_map(|o| match &o.details {
            ObjectDetails::ForeignKey(fk) => Some(fk.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("{engine}: no foreign key listed"));
    let children = query(
        h,
        &format!("SELECT id, parent_id FROM {} ORDER BY id", n.child),
        vec![],
    )
    .await;
    let values = row_values_by_name(&children[0], 0, &fk.columns).unwrap();
    let parent_rows = built(
        h,
        referenced_rows_query(&*h.dialect(), &fk, &values).unwrap(),
    )
    .await;
    assert_eq!(row_count(&parent_rows), 1, "{engine}");
    let incoming = edit
        .referencing_foreign_keys(h, parent_path.clone())
        .await
        .unwrap()
        .unwrap();
    assert!(
        incoming
            .keys
            .iter()
            .any(|(t, k)| *t == child_path && *k == fk),
        "{engine}: the child's foreign key is not found from the parent"
    );
    let parent_row = query(h, &select, vec![]).await;
    let key = row_values_by_name(&parent_row[0], 0, &fk.referenced_columns).unwrap();
    let referencing = built(
        h,
        referencing_rows_query(&*h.dialect(), &child_path, &fk, &key).unwrap(),
    )
    .await;
    assert_eq!(row_count(&referencing), 2, "{engine}");

    // "SQL INSERT" export through the P5-B renderer, replayed into a copy of the table.
    let transfer = TransferService::new(manager.clone(), &paths(&root.join("ninesql")));
    let out = root.join(format!("{engine}-export.sql"));
    let mut options = ExportOptions::new(ExportFormat::SqlInsert);
    options.sql.table = vec![n.copy.clone()];
    options.sql.rows_per_statement = 2;
    let mut job = transfer.export(
        h,
        ExportRequest::statement(
            format!("SELECT id, label FROM {} ORDER BY id", n.parent),
            &out,
            options,
        ),
    );
    loop {
        match job.next().await {
            Some(ExportEvent::Done(s)) => {
                assert_eq!(s.rows, 3);
                break;
            }
            Some(ExportEvent::Failed(e)) => panic!("{engine}: {e}"),
            Some(_) => {}
            None => panic!("ended"),
        }
    }
    // Wait for the export's read-only transaction to end.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while h.is_leased() || h.transaction_state().in_transaction {
        assert!(std::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let script = std::fs::read_to_string(&out).unwrap();
    if engine == "postgresql" {
        // The literals read the same in either lexical mode (security review H1 of P5-B): the
        // file is replayed with backslash escapes on.
        exec_ok(h, "SET standard_conforming_strings = off").await;
    }
    for span in h.dialect().split_statements(&script) {
        exec_ok(h, &script[span.start..span.end]).await;
    }
    if engine == "postgresql" {
        exec_ok(h, "SET standard_conforming_strings = on").await;
    }
    let copied = query(
        h,
        &format!("SELECT id, label FROM {} ORDER BY id", n.copy),
        vec![],
    )
    .await;
    let original = query(h, &select, vec![]).await;
    assert_eq!(row_count(&copied), 3);
    for r in 0..3 {
        assert_eq!(int(&copied[0], 0, r), int(&original[0], 0, r));
        assert_eq!(text(&copied[0], 1, r), text(&original[0], 1, r), "{engine}");
    }

    // Manual-commit mode: the save runs inside the session's transaction around a savepoint.
    // A failed save undoes only itself; a good one stays uncommitted until the user commits.
    h.set_auto_commit(false).await.unwrap().unwrap();
    // Earlier uncommitted work of the session, which a failed save must not discard.
    exec_ok(
        h,
        &format!("UPDATE {} SET label = 'pending' WHERE id = 4", n.parent),
    )
    .await;
    let rows = query(h, &select, vec![]).await;
    let mut good = PendingEdits::new(table.clone(), &source, &rows[0].schema()).unwrap();
    good.set_value(1, &rows[0], 1, 1, Param::Text("manual".into()))
        .unwrap();
    let mut bad = good.clone();
    bad.set_value(9, b, 0, 1, Param::Text("stale again".into()))
        .unwrap();
    match end(edit.save(h, &bad, SaveOptions::default())).await {
        SaveEvent::Failed(EditError::RowCount { .. }) => {}
        other => panic!("{engine}: {other:?}"),
    }
    assert!(
        h.transaction_state().in_transaction,
        "{engine}: the session's transaction is kept"
    );
    assert_eq!(
        label(h, &n.parent, 2).await.as_deref(),
        Some("was null"),
        "{engine}"
    );
    assert_eq!(
        label(h, &n.parent, 4).await.as_deref(),
        Some("pending"),
        "{engine}: the earlier uncommitted work was lost"
    );
    let SaveEvent::Done(s) = end(edit.save(h, &good, SaveOptions::default())).await else {
        panic!("{engine}: not saved in manual-commit mode");
    };
    assert!(!s.committed);
    assert!(h.transaction_state().in_transaction, "{engine}");
    assert_eq!(label(h, &n.parent, 2).await.as_deref(), Some("manual"));
    h.commit().await.unwrap().unwrap();
    h.set_auto_commit(true).await.unwrap().unwrap();
    assert_eq!(
        label(h, &n.parent, 4).await.as_deref(),
        Some("pending"),
        "{engine}"
    );
    assert_eq!(
        label(h, &n.parent, 2).await.as_deref(),
        Some("manual"),
        "{engine}"
    );
}

fn ddl(n: &Names, label_type: &str) -> Vec<String> {
    vec![
        format!(
            "CREATE TABLE {} (id integer PRIMARY KEY, label {label_type})",
            n.parent
        ),
        format!(
            "CREATE TABLE {} (id integer PRIMARY KEY, parent_id integer, FOREIGN KEY (parent_id) REFERENCES {} (id))",
            n.child, n.parent
        ),
        format!(
            "CREATE TABLE {} (id integer PRIMARY KEY, label {label_type})",
            n.copy
        ),
        format!(
            "INSERT INTO {} (id, label) VALUES (1, 'a'), (2, NULL), (3, 'c')",
            n.parent
        ),
        format!(
            "INSERT INTO {} (id, parent_id) VALUES (10, 1), (11, 1), (12, 2)",
            n.child
        ),
    ]
}

fn server_engine(engine: &str, driver: Arc<dyn Driver>) {
    let Some(dev) = dev(engine) else { return };
    let tmp = tempfile::tempdir().unwrap();
    let (manager, _) = manager(driver, tmp.path());
    let n = names();
    ui(async {
        let h = open_with(&manager, tcp_connection(&dev, engine), Some(&dev.password)).await;
        for t in [&n.child, &n.copy, &n.parent] {
            let _ = run(&h, &format!("DROP TABLE IF EXISTS {t}")).await;
        }
        for s in ddl(&n, "varchar(100)") {
            exec_ok(&h, &s).await;
        }
        let schema = if engine == "postgresql" {
            "public".to_owned()
        } else {
            dev.database.clone()
        };
        scenario(&manager, &h, &schema, &n, tmp.path()).await;
        for t in [&n.child, &n.copy, &n.parent] {
            exec_ok(&h, &format!("DROP TABLE {t}")).await;
        }
    });
}

#[test]
fn postgresql_edit_filter_navigate_export() {
    server_engine(
        "postgresql",
        Arc::new(ninesql_driver_pgwire::PgWireDriver::new()),
    );
}

#[test]
fn mysql_edit_filter_navigate_export() {
    server_engine(
        "mysql",
        Arc::new(ninesql_driver_mysqlwire::MySqlWireDriver::new()),
    );
}

#[test]
fn mariadb_edit_filter_navigate_export() {
    server_engine(
        "mariadb",
        Arc::new(ninesql_driver_mysqlwire::MySqlWireDriver::new()),
    );
}

#[test]
fn sqlite_edit_filter_navigate_export() {
    let tmp = tempfile::tempdir().unwrap();
    let driver = ninesql_driver_sqlite::SqliteDriver::new();
    let engine = driver.engines()[0].clone();
    let (manager, _) = manager(Arc::new(driver), tmp.path());
    let db = tmp.path().join("edit.db");
    let n = names();
    let conn = rusqlite::Connection::open(&db).unwrap();
    for s in ddl(&n, "TEXT") {
        conn.execute_batch(&s).unwrap();
    }
    drop(conn);
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
        scenario(&manager, &h, "main", &n, tmp.path()).await;
    });
}
