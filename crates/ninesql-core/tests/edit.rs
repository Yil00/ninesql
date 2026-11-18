//! Edit service (P5-A core) on the mock driver: the save's transaction and lease, the production
//! confirmation of the whole save, the per-statement confirmation of confirm-every-write types,
//! and the refusals (read-only, open transaction).
#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).

mod support;

use std::sync::Arc;
use std::time::Duration;

use ninesql_core::driver_api::AnsiDialect;
use ninesql_core::driver_api::arrow_array::RecordBatch;
use ninesql_core::driver_api::mock::MOCK_ENGINE;
use ninesql_core::driver_api::{
    ColumnInfo, IndexInfo, ObjectDetails, ObjectInfo, ObjectKind, ObjectPath, Param, ResultItem,
    TableInfo,
};
use ninesql_core::edit::{MAX_CHANGES_PER_SAVE, QuerySource, TableQuery, build_table_query};
use ninesql_core::{
    ConnectionType, ConnectionTypeId, ConnectionTypes, EditError, EditService, EditableTable,
    ExecEvent, ExecutionService, MetadataConfig, MetadataService, NotEditable, PendingEdits,
    Preferences, SaveEvent, SaveJob, SaveOptions, SessionHandle,
};
use support::*;

fn path() -> ObjectPath {
    ObjectPath::root()
        .child(ObjectKind::Catalog, "nine")
        .child(ObjectKind::Schema, "public")
        .child(ObjectKind::Table, "t000")
}

fn table() -> EditableTable {
    let col = |name: &str, ordinal: u32, ty: &str| {
        let mut c = ColumnInfo::new(ordinal, ty);
        c.nullable = name != "id";
        c.primary_key = name == "id";
        ObjectInfo {
            path: path().child(ObjectKind::Column, name),
            comment: None,
            details: ObjectDetails::Column(c),
        }
    };
    let mut pk = IndexInfo::default();
    pk.columns = vec!["id".into()];
    pk.expressions = vec![false];
    pk.primary = true;
    pk.unique = true;
    pk.valid = true;
    let idx = ObjectInfo {
        path: path().child(ObjectKind::Index, "pk"),
        comment: None,
        details: ObjectDetails::Index(pk),
    };
    let mut info = TableInfo::default();
    info.transactional = Some(true);
    EditableTable::from_metadata(
        &MOCK_ENGINE,
        path(),
        Some(&info),
        &[col("id", 1, "bigint"), col("label", 2, "text")],
        true,
        &[idx],
    )
    .unwrap()
}

fn service(manager: &ninesql_core::SessionManager) -> EditService {
    EditService::new(
        manager.clone(),
        MetadataService::new(manager.clone(), MetadataConfig::default()),
        ExecutionService::new(manager.clone(), None, Preferences::default()),
    )
}

async fn rows(h: &SessionHandle) -> RecordBatch {
    let events = run(h, "SELECT * FROM numbers(3)").await;
    events
        .into_iter()
        .find_map(|e| match e {
            ExecEvent::Item(ResultItem::Rows(b)) => Some(b),
            _ => None,
        })
        .unwrap()
}

/// The source of the navigator's "open table" query of `t000` (`SELECT *`).
fn source() -> QuerySource {
    build_table_query(&AnsiDialect, &TableQuery::of_path(&path()))
        .unwrap()
        .source()
        .clone()
}

async fn edits(h: &SessionHandle) -> (PendingEdits, RecordBatch) {
    let b = rows(h).await;
    let mut p = PendingEdits::new(table(), &source(), &b.schema()).unwrap();
    p.set_value(0, &b, 0, 1, Param::Text("renamed".into()))
        .unwrap();
    p.delete_row(2, &b, 2).unwrap();
    let n = p.insert_row();
    p.set_new_value(n, 1, Some(Param::Text("new".into())))
        .unwrap();
    (p, b)
}

async fn end(mut job: SaveJob) -> SaveEvent {
    loop {
        match tokio::time::timeout(Duration::from_secs(20), job.next())
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

#[test]
fn dev_save_runs_in_one_transaction_and_releases_the_session() {
    let dir = tempfile::tempdir().unwrap();
    let (manager, _) = manager(Arc::new(mock_driver()), dir.path());
    let edit = service(&manager);
    ui(async {
        let h = open_ok(
            &manager,
            mock_connection(ConnectionTypeId::dev()),
            &ConnectionTypes::default(),
        )
        .await;
        let (p, _) = edits(&h).await;
        let plan = p
            .plan(&*h.dialect(), &h.info().capabilities.editing)
            .unwrap();
        assert_eq!(plan.batch.statements.len(), 3);
        assert!(
            plan.script
                .starts_with("DELETE FROM \"nine\".\"public\".\"t000\" WHERE \"id\" = 2")
        );
        let SaveEvent::Done(summary) = end(edit.save(&h, &p, SaveOptions::default())).await else {
            panic!("not saved");
        };
        assert_eq!(summary.statements, 3);
        assert_eq!(summary.rows_affected, 3);
        assert!(!h.is_leased());
        assert!(!h.transaction_state().in_transaction);
        assert!(summary.committed);
        // In auto-commit mode an open transaction is refused: the save owns its transaction.
        h.begin().await.unwrap().unwrap();
        assert!(matches!(
            end(edit.save(&h, &p, SaveOptions::default())).await,
            SaveEvent::Failed(EditError::PendingTransaction)
        ));
        h.rollback().await.unwrap().unwrap();
        // In manual-commit mode the save runs inside the session's transaction and leaves it
        // open for the user to commit.
        h.set_auto_commit(false).await.unwrap().unwrap();
        h.begin().await.unwrap().unwrap();
        let SaveEvent::Done(summary) = end(edit.save(&h, &p, SaveOptions::default())).await else {
            panic!("not saved");
        };
        assert!(!summary.committed);
        assert!(h.transaction_state().in_transaction);
        assert!(!h.is_leased());
        h.rollback().await.unwrap().unwrap();
    });
}

#[test]
fn production_confirms_the_whole_save_once() {
    let dir = tempfile::tempdir().unwrap();
    let (manager, _) = manager(Arc::new(mock_driver()), dir.path());
    let edit = service(&manager);
    ui(async {
        let h = open_ok(
            &manager,
            mock_connection(ConnectionTypeId::prod()),
            &ConnectionTypes::default(),
        )
        .await;
        let (p, b) = edits(&h).await;
        let SaveEvent::NeedsConfirmation(c) = end(edit.save(&h, &p, SaveOptions::default())).await
        else {
            panic!("no confirmation on production");
        };
        assert!(c.production);
        assert_eq!(c.statements, 3);
        assert!(c.script.contains("'renamed'"));
        assert!(!format!("{c:?}").contains("renamed"));
        // A token is bound to the exact changes: other changes are refused, and it is spent.
        let mut other = p.clone();
        other
            .set_value(1, &b, 1, 1, Param::Text("x".into()))
            .unwrap();
        assert!(matches!(
            end(edit.save(&h, &other, SaveOptions::default().confirmed(c.token))).await,
            SaveEvent::Failed(EditError::InvalidConfirmation)
        ));
        let SaveEvent::NeedsConfirmation(c) = end(edit.save(&h, &p, SaveOptions::default())).await
        else {
            panic!("no confirmation");
        };
        let SaveEvent::Done(s) =
            end(edit.save(&h, &p, SaveOptions::default().confirmed(c.token))).await
        else {
            panic!("not saved");
        };
        assert_eq!(s.statements, 3);
        // Production is manual-commit: the changes wait in the open transaction.
        assert!(!s.committed);
        assert!(h.transaction_state().in_transaction);
        h.commit().await.unwrap().unwrap();
        assert!(matches!(
            end(edit.save(&h, &p, SaveOptions::default().confirmed(c.token))).await,
            SaveEvent::Failed(EditError::InvalidConfirmation)
        ));
    });
}

#[test]
fn confirm_every_write_type_answers_only_the_confirmed_statements() {
    let dir = tempfile::tempdir().unwrap();
    let (manager, _) = manager(Arc::new(mock_driver()), dir.path());
    let edit = service(&manager);
    let mut types = ConnectionTypes::default();
    let mut strict = ConnectionType::test();
    strict.id = ConnectionTypeId::new("strict").unwrap();
    strict.confirm_every_write = true;
    types.upsert(strict);
    ui(async {
        let h = open_ok(
            &manager,
            mock_connection(ConnectionTypeId::new("strict").unwrap()),
            &types,
        )
        .await;
        let (p, _) = edits(&h).await;
        let SaveEvent::NeedsConfirmation(c) = end(edit.save(&h, &p, SaveOptions::default())).await
        else {
            panic!("no confirmation on a confirm-every-write type");
        };
        assert!(!c.production);
        let SaveEvent::Done(s) =
            end(edit.save(&h, &p, SaveOptions::default().confirmed(c.token))).await
        else {
            panic!("not saved");
        };
        assert_eq!(s.rows_affected, 3);
    });
}

#[test]
fn read_only_sessions_never_save() {
    let dir = tempfile::tempdir().unwrap();
    let (manager, _) = manager(Arc::new(mock_driver()), dir.path());
    let edit = service(&manager);
    ui(async {
        let mut c = mock_connection(ConnectionTypeId::dev());
        c.read_only = Some(true);
        let h = open_ok(&manager, c, &ConnectionTypes::default()).await;
        let (p, _) = edits(&h).await;
        assert!(matches!(
            end(edit.save(&h, &p, SaveOptions::default())).await,
            SaveEvent::Failed(EditError::NotEditable(NotEditable::ReadOnlySession))
        ));
        assert!(matches!(
            edit.describe_table(&h, path()).await.unwrap(),
            Err(EditError::NotEditable(NotEditable::ReadOnlySession))
        ));
    });
}

#[test]
fn describe_table_without_index_has_no_key() {
    let dir = tempfile::tempdir().unwrap();
    let (manager, _) = manager(Arc::new(mock_driver()), dir.path());
    let edit = service(&manager);
    ui(async {
        let h = open_ok(
            &manager,
            mock_connection(ConnectionTypeId::dev()),
            &ConnectionTypes::default(),
        )
        .await;
        // The mock lists no indexes: no key, so the grid stays read-only with an explanation.
        match edit.describe_table(&h, path()).await.unwrap() {
            Err(EditError::NotEditable(NotEditable::NoKey)) => {}
            other => panic!("{other:?}"),
        }
        // While a save is not running, other statements work.
        assert!(matches!(
            terminal(&run(&h, "SELECT * FROM numbers(1)").await),
            ExecEvent::Finished { .. }
        ));
    });
}

#[test]
fn a_save_that_lost_the_sessions_transaction_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let wrap = Wrap::new(mock_driver());
    let (manager, _) = manager(Arc::new(wrap.clone()), dir.path());
    let edit = service(&manager);
    ui(async {
        let h = open_ok(
            &manager,
            mock_connection(ConnectionTypeId::dev()),
            &ConnectionTypes::default(),
        )
        .await;
        let (p, _) = edits(&h).await;
        h.set_auto_commit(false).await.unwrap().unwrap();
        h.begin().await.unwrap().unwrap();
        // The second statement fails, and so does `ROLLBACK TO SAVEPOINT`: the save can only
        // be undone with the whole transaction, which discards the earlier uncommitted work.
        // The failure says so instead of looking like a plain failure.
        *wrap.fail_prefixes.lock().unwrap() = vec!["UPDATE".into(), "ROLLBACK TO".into()];
        match end(edit.save(&h, &p, SaveOptions::default())).await {
            SaveEvent::Failed(e @ EditError::TransactionRolledBack { .. }) => {
                assert!(
                    matches!(&e, EditError::TransactionRolledBack { source } if matches!(**source, EditError::Core(_)))
                );
                assert!(e.to_string().contains("earlier uncommitted changes"));
                assert_eq!(e.kind(), "transaction-rolled-back");
            }
            other => panic!("{other:?}"),
        }
        assert!(!h.transaction_state().in_transaction);
        assert!(!h.is_leased());
        // A failed statement whose undo works is a plain failure (the transaction would be
        // kept on an engine; the mock's `ROLLBACK TO` ends it, which is reported the same way).
        *wrap.fail_prefixes.lock().unwrap() = vec!["UPDATE".into()];
        h.set_auto_commit(true).await.unwrap().unwrap();
        assert!(matches!(
            end(edit.save(&h, &p, SaveOptions::default())).await,
            SaveEvent::Failed(EditError::Core(_))
        ));
        assert!(!h.transaction_state().in_transaction);
    });
}

/// P5-E L2: when neither `ROLLBACK TO SAVEPOINT` nor the fallback `ROLLBACK` works, part of the
/// save could stay in the session's open transaction: the service closes the session (which
/// rolls back) instead of leaving it for a later commit.
#[test]
fn a_save_that_cannot_be_undone_closes_the_session() {
    let dir = tempfile::tempdir().unwrap();
    let wrap = Wrap::new(mock_driver());
    let (manager, _) = manager(Arc::new(wrap.clone()), dir.path());
    let edit = service(&manager);
    ui(async {
        let h = open_ok(
            &manager,
            mock_connection(ConnectionTypeId::dev()),
            &ConnectionTypes::default(),
        )
        .await;
        let (p, _) = edits(&h).await;
        h.set_auto_commit(false).await.unwrap().unwrap();
        h.begin().await.unwrap().unwrap();
        *wrap.fail_prefixes.lock().unwrap() = vec!["UPDATE".into(), "ROLLBACK TO".into()];
        wrap.fail_rollback
            .store(true, std::sync::atomic::Ordering::SeqCst);
        match end(edit.save(&h, &p, SaveOptions::default())).await {
            SaveEvent::Failed(e @ EditError::UndoFailed { .. }) => {
                assert_eq!(e.kind(), "undo-failed");
                assert!(e.to_string().contains("the session was closed"));
            }
            other => panic!("{other:?}"),
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !h.is_closed() {
            assert!(
                std::time::Instant::now() < deadline,
                "the session was not closed"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // Nothing can run on it any more, a commit included.
        assert!(h.commit().await.is_none_or(|r| r.is_err()));
    });
}

#[test]
fn manual_commit_without_savepoints_is_refused_and_saves_are_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let mut caps = mock_capabilities();
    caps.transactions.savepoints = false;
    let driver = ninesql_core::driver_api::mock::MockDriver::new().with_capabilities(caps);
    let (manager, _) = manager(Arc::new(driver), dir.path());
    let edit = service(&manager);
    ui(async {
        let h = open_ok(
            &manager,
            mock_connection(ConnectionTypeId::dev()),
            &ConnectionTypes::default(),
        )
        .await;
        let (p, _) = edits(&h).await;
        h.set_auto_commit(false).await.unwrap().unwrap();
        assert!(matches!(
            end(edit.save(&h, &p, SaveOptions::default())).await,
            SaveEvent::Failed(EditError::NoSavepoints)
        ));
        // Auto-commit still saves (a transaction of its own).
        h.set_auto_commit(true).await.unwrap().unwrap();
        assert!(matches!(
            end(edit.save(&h, &p, SaveOptions::default())).await,
            SaveEvent::Done(_)
        ));
        let mut big = p.clone();
        for _ in 0..MAX_CHANGES_PER_SAVE {
            big.insert_row();
        }
        assert!(matches!(
            big.plan(&*h.dialect(), &h.info().capabilities.editing),
            Err(EditError::TooManyChanges { .. })
        ));
    });
}
