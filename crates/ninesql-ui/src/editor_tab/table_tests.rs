//! `TestAppContext` tests of table editing, server-side filters and the DBeaver export dialog
//! on the core's mock driver (P5-A): a typed query and a table without key stay read-only with
//! their reason; a production save shows the exact script and resumes with the token; a
//! refused raw `WHERE` keeps the query; the actions are in the registry.

use gpui::{Entity, TestAppContext, VisualTestContext};
use gpui_kit::component::WindowExt as _;
use ninesql_core::driver_api::metadata::{ObjectDetails, ObjectInfo, ObjectPath};
use ninesql_core::driver_api::mock::MOCK_ENGINE;
use ninesql_core::driver_api::{ColumnInfo, IndexInfo, ObjectKind};
use ninesql_core::{Auth, ConnectionId, ConnectionTypeId, EditableTable, SessionHandle};
use ninesql_editor::execution::ExecutionKind;

use super::SqlEditorTab;
use super::table::{TYPED_READ_ONLY, TableSource};
use crate::connections::connect_flow::Purpose;
use crate::connections::hub::ConnectionHub;
use crate::connections::tests::{mock_connection, mock_hub_with_transfer, open_live, wait_until};

struct Live {
    tab: Entity<SqlEditorTab>,
    session: SessionHandle,
    vcx: VisualTestContext,
    _hub: Entity<ConnectionHub>,
}

fn live(cx: &mut TestAppContext, ty: ConnectionTypeId, dir: &std::path::Path) -> Live {
    let c = mock_connection("Live", ty, Auth::None);
    let id: ConnectionId = c.id;
    let hub = mock_hub_with_transfer(cx, dir, vec![c]);
    let (ws, mut vcx) = open_live(cx);
    ws.update_in(&mut vcx, |ws, window, cx| {
        ws.connect(id, Purpose::Open, window, cx);
    });
    wait_until(&mut vcx, |vcx| hub.read_with(vcx, |h, _| h.is_open(id)));
    let session = hub.read_with(&vcx, |h, _| h.sessions(id).first().cloned());
    let tab = ws.read_with(&vcx, |ws, _| ws.active_editor().cloned());
    let (Some(session), Some(tab)) = (session, tab) else {
        panic!("a session and an editor tab");
    };
    wait_until(&mut vcx, |vcx| {
        tab.read_with(vcx, |t, _| t.connection() == Some(id))
    });
    Live {
        tab,
        session,
        vcx,
        _hub: hub,
    }
}

fn path() -> ObjectPath {
    ObjectPath::root()
        .child(ObjectKind::Catalog, "nine")
        .child(ObjectKind::Schema, "public")
        .child(ObjectKind::Table, "t000")
}

/// `t000` (`id` primary key, `label`) with a key, as a driver with indexes would describe it.
fn keyed_table() -> EditableTable {
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
    EditableTable::from_metadata(
        &MOCK_ENGINE,
        path(),
        None,
        &[col("id", 1, "bigint"), col("label", 2, "text")],
        true,
        &[idx],
    )
    .unwrap_or_else(|e| panic!("{e}"))
}

/// Opens `t000` with a first query the mock answers (`numbers(3)`: `id`, `label`), waits for
/// the run, then gives the tab the keyed description (the mock lists no index).
fn open_editable(l: &mut Live) {
    let mut source = TableSource::of_path(path());
    source.initial = Some(("SELECT * FROM numbers(3)".to_owned(), Vec::new()));
    let tab = l.tab.clone();
    l.vcx
        .update(|window, cx| tab.update(cx, |t, cx| t.open_table(source, window, cx)));
    wait_until(&mut l.vcx, |vcx| {
        tab.read_with(vcx, |t, cx| {
            !t.is_running()
                && t.table_state().describing.is_none()
                && t.results().read(cx).tab_of_statement(0).is_some()
        })
    });
    l.vcx
        .update(|_, cx| tab.update(cx, |t, cx| t.set_description(Ok(keyed_table()), cx)));
    assert!(
        l.tab.read_with(&l.vcx, |t, _| t.editing().is_some()),
        "{:?}",
        l.tab
            .read_with(&l.vcx, |t, _| t.read_only_reason().map(str::to_owned))
    );
}

fn rename_first_row(l: &mut Live) {
    let tab = l.tab.clone();
    let r = l
        .vcx
        .update(|_, cx| tab.update(cx, |t, cx| t.set_cell(0, 1, Some("renamed"), cx)));
    assert_eq!(r, Ok(()));
    let ix = tab.read_with(&l.vcx, |t, _| t.editing().map(|e| e.tab));
    let dirty = tab.read_with(&l.vcx, |t, cx| {
        ix.and_then(|ix| t.results().read(cx).edit_buffer(ix).cloned())
            .is_some_and(|b| b.is_cell_dirty(0, 1))
    });
    assert!(dirty, "the grid shows the dirty marker");
}

#[gpui::test]
fn typed_query_is_read_only_with_its_reason(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let mut l = live(cx, ConnectionTypeId::dev(), tmp.path());
    let editor = l.tab.read_with(&l.vcx, |t, _| t.editor().clone());
    l.vcx.update(|window, cx| {
        editor.update(cx, |e, cx| {
            e.set_text("SELECT * FROM numbers(3)", window, cx);
            e.execute(ExecutionKind::Statement, window, cx);
        });
    });
    let tab = l.tab.clone();
    wait_until(&mut l.vcx, |vcx| {
        tab.read_with(vcx, |t, cx| {
            !t.is_running() && t.results().read(cx).tab_of_statement(0).is_some()
        })
    });
    assert_eq!(
        l.tab
            .read_with(&l.vcx, |t, _| t.read_only_reason().map(str::to_owned)),
        Some(TYPED_READ_ONLY.to_owned())
    );
    // Editing actions are refused with the reason.
    let refused = l
        .vcx
        .update(|_, cx| tab.update(cx, |t, cx| t.cell_to_edit(cx)));
    assert!(refused.is_none());
    let message = l
        .tab
        .read_with(&l.vcx, |t, _| t.message().map(|m| m.text.to_string()));
    assert_eq!(message.as_deref(), Some(TYPED_READ_ONLY));
}

#[gpui::test]
fn table_without_key_stays_read_only_with_its_reason(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let mut l = live(cx, ConnectionTypeId::dev(), tmp.path());
    let tab = l.tab.clone();
    l.vcx.update(|window, cx| {
        tab.update(cx, |t, cx| {
            t.open_table(TableSource::of_path(path()), window, cx)
        });
    });
    // The mock lists no index: `describe_table` answers `NotEditable::NoKey`.
    wait_until(&mut l.vcx, |vcx| {
        tab.read_with(vcx, |t, _| {
            !t.is_running() && t.table_state().described.is_some()
        })
    });
    let (editable, reason) = l.tab.read_with(&l.vcx, |t, _| {
        (
            t.editing().is_some(),
            t.read_only_reason().map(str::to_owned),
        )
    });
    assert!(!editable);
    let reason = reason.unwrap_or_default();
    assert!(reason.contains("no primary key or unique key"), "{reason}");
    // The query the editor shows is the core's, quoted by the dialect.
    let text = l
        .tab
        .read_with(&l.vcx, |t, cx| t.editor().read(cx).text(cx));
    assert_eq!(text, "SELECT * FROM \"public\".\"t000\"");
    // No dirty marker can be set.
    let r = l
        .vcx
        .update(|_, cx| tab.update(cx, |t, cx| t.set_cell(0, 1, Some("x"), cx)));
    assert!(r.is_err());
}

#[gpui::test]
fn production_save_shows_the_exact_script_and_resumes_with_the_token(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let mut l = live(cx, ConnectionTypeId::prod(), tmp.path());
    open_editable(&mut l);
    rename_first_row(&mut l);
    let tab = l.tab.clone();
    let plan = l
        .vcx
        .update(|_, cx| tab.update(cx, |t, cx| t.plan_changes(cx)))
        .unwrap_or_else(|e| panic!("{e}"));
    assert!(plan.script.contains("UPDATE"), "{}", plan.script);
    assert!(plan.script.contains("renamed"), "{}", plan.script);

    l.vcx
        .update(|window, cx| tab.update(cx, |t, cx| t.save_changes(window, cx)));
    wait_until(&mut l.vcx, |vcx| {
        tab.read_with(vcx, |t, _| t.confirmation().is_some())
    });
    let (script, production) = l.tab.read_with(&l.vcx, |t, _| {
        let c = t.confirmation().unwrap_or_else(|| panic!("a confirmation"));
        (c.script.clone(), c.production)
    });
    assert!(production);
    assert_eq!(script, plan.script, "the dialog shows the exact script");
    assert!(l.vcx.update(|window, cx| window.has_active_dialog(cx)));
    // Nothing ran: the edit is still pending.
    assert!(l.tab.read_with(&l.vcx, |t, _| t.has_pending_edits()));

    l.vcx.update(|window, cx| {
        window.close_dialog(cx);
        tab.update(cx, |t, cx| t.confirm_save(window, cx));
    });
    wait_until(&mut l.vcx, |vcx| {
        tab.read_with(vcx, |t, _| {
            !t.table_state().saving && !t.has_pending_edits()
        })
    });
    let message = l
        .tab
        .read_with(&l.vcx, |t, _| t.message().map(|m| m.text.to_string()))
        .unwrap_or_default();
    // Production is manual-commit: the save stays in the open transaction.
    assert!(message.contains("commit to keep them"), "{message}");
    assert!(l.session.transaction_state().in_transaction);
}

#[gpui::test]
fn declined_confirmation_saves_nothing(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let mut l = live(cx, ConnectionTypeId::prod(), tmp.path());
    open_editable(&mut l);
    rename_first_row(&mut l);
    let tab = l.tab.clone();
    l.vcx
        .update(|window, cx| tab.update(cx, |t, cx| t.save_changes(window, cx)));
    wait_until(&mut l.vcx, |vcx| {
        tab.read_with(vcx, |t, _| t.confirmation().is_some())
    });
    // Escape / closing the dialog without an answer declines.
    l.vcx.update(|window, cx| window.close_dialog(cx));
    wait_until(&mut l.vcx, |vcx| {
        tab.read_with(vcx, |t, _| t.confirmation().is_none())
    });
    assert!(l.tab.read_with(&l.vcx, |t, _| t.has_pending_edits()));
    assert!(!l.session.transaction_state().in_transaction);
}

#[gpui::test]
fn dev_save_commits_and_inserted_rows_are_planned(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let mut l = live(cx, ConnectionTypeId::dev(), tmp.path());
    open_editable(&mut l);
    let tab = l.tab.clone();
    let row = l.vcx.update(|_, cx| {
        tab.update(cx, |t, cx| {
            t.insert_row_with(vec![(1, Some("new".into()))], cx)
        })
    });
    assert!(row.is_ok());
    let bad = l.vcx.update(|_, cx| {
        tab.update(cx, |t, cx| {
            t.insert_row_with(vec![(0, Some("not a number".into()))], cx)
        })
    });
    assert!(
        bad.is_err(),
        "the core converts the input to the column's type"
    );
    let plan = l
        .vcx
        .update(|_, cx| tab.update(cx, |t, cx| t.plan_changes(cx)))
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(plan.inserts, 1);
    l.vcx
        .update(|window, cx| tab.update(cx, |t, cx| t.save_changes(window, cx)));
    wait_until(&mut l.vcx, |vcx| {
        tab.read_with(vcx, |t, _| {
            !t.table_state().saving && !t.has_pending_edits()
        })
    });
    let message = l
        .tab
        .read_with(&l.vcx, |t, _| t.message().map(|m| m.text.to_string()))
        .unwrap_or_default();
    assert!(message.starts_with("Saved 1 changes."), "{message}");
    assert!(!l.session.transaction_state().in_transaction);
}

#[gpui::test]
fn refused_raw_where_keeps_the_query_and_filters_are_recorded(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let mut l = live(cx, ConnectionTypeId::dev(), tmp.path());
    open_editable(&mut l);
    let tab = l.tab.clone();
    let refused = l.vcx.update(|window, cx| {
        tab.update(cx, |t, cx| {
            t.set_raw_where(Some("1=1; DROP TABLE t000".into()), window, cx)
        })
    });
    let why = refused.err().unwrap_or_default();
    assert!(why.contains("WHERE expression was refused"), "{why}");
    assert!(l.tab.read_with(&l.vcx, |t, _| {
        t.table_state()
            .source
            .as_ref()
            .is_some_and(|s| s.query.raw_where.is_none())
    }));
    let ok = l.vcx.update(|window, cx| {
        tab.update(cx, |t, cx| {
            t.set_raw_where(Some("id > 1".into()), window, cx)
        })
    });
    assert_eq!(ok, Ok(()));
    let text = l
        .tab
        .read_with(&l.vcx, |t, cx| t.editor().read(cx).text(cx));
    assert!(text.contains("WHERE (\nid > 1\n)"), "{text}");
    let history = l
        .vcx
        .update(|_, cx| tab.update(cx, |t, cx| t.filter_history(cx)));
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].raw_where.as_deref(), Some("id > 1"));
}

#[gpui::test]
fn pending_edits_block_a_new_run(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let mut l = live(cx, ConnectionTypeId::dev(), tmp.path());
    open_editable(&mut l);
    rename_first_row(&mut l);
    let tab = l.tab.clone();
    let ran = l
        .vcx
        .update(|window, cx| tab.update(cx, |t, cx| t.run_source(window, cx)));
    assert!(!ran);
    l.vcx
        .update(|_, cx| tab.update(cx, |t, cx| t.discard_changes(cx)));
    assert!(!l.tab.read_with(&l.vcx, |t, _| t.has_pending_edits()));
}

#[test]
fn table_actions_have_keys_and_palette_entries() {
    for name in [
        "OpenTableData",
        "EditCell",
        "SetCellNull",
        "InsertRow",
        "DuplicateRow",
        "DeleteRows",
        "RevertRowChanges",
        "DiscardChanges",
        "PreviewChanges",
        "SaveChanges",
        "FilterTableRows",
        "FilterTableBySelectedValue",
        "SortTableAscending",
        "SortTableDescending",
        "ClearTableFilter",
        "RefreshTableData",
        "OpenReferencedRows",
        "OpenReferencingRows",
        "ExportToDbeaver",
    ] {
        let spec = crate::actions::find(name).unwrap_or_else(|| panic!("{name}"));
        assert!(spec.in_palette, "{name}");
        assert!(!spec.default_keys.is_empty(), "{name}");
    }
}
