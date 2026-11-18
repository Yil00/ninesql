//! `TestAppContext` tests of table editing, server-side filters and the DBeaver export dialog
//! on the core's mock driver (P5-A): a typed query and a table without key stay read-only with
//! their reason; a production save shows the exact script and resumes with the token; a
//! refused raw `WHERE` keeps the query; the actions are in the registry.

use gpui::{Entity, TestAppContext, VisualTestContext};
use gpui_kit::component::WindowExt as _;
use ninesql_core::driver_api::metadata::{ObjectDetails, ObjectInfo, ObjectPath, TableInfo};
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
    .unwrap_or_else(|e| panic!("{e}"))
}

/// Opens `t000`: the core builds its query (and provenance); the mock runs `text` instead.
fn open_with_text(l: &mut Live, text: &str) {
    let tab = l.tab.clone();
    let text = text.to_owned();
    l.vcx.update(|window, cx| {
        tab.update(cx, |t, cx| {
            t.table.run_text_override = Some(text);
            t.open_table(TableSource::of_path(path()), window, cx);
        });
    });
}

/// Opens `t000` with a first query the mock answers (`numbers(3)`: `id`, `label`), waits for
/// the run, then gives the tab the keyed description (the mock lists no index).
fn open_editable(l: &mut Live) {
    open_with_text(l, "SELECT * FROM numbers(3)");
    let tab = l.tab.clone();
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
    assert_eq!(text, "SELECT * FROM \"nine\".\"public\".\"t000\"");
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
    // "Revert row changes" cannot change what the confirmation shows (review L2).
    l.vcx
        .update(|_, cx| tab.update(cx, |t, cx| t.revert_selected_rows(cx)));
    assert!(l.tab.read_with(&l.vcx, |t, _| t.has_pending_edits()));
    let said = l
        .tab
        .read_with(&l.vcx, |t, _| t.message().map(|m| m.text.to_string()))
        .unwrap_or_default();
    assert!(said.contains("waits for its confirmation"), "{said}");

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
            t.table.run_text_override = None;
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

/// The rows loaded in the table tab's first result.
fn loaded_rows(l: &Live) -> usize {
    l.tab.read_with(&l.vcx, |t, cx| {
        match t.results().read(cx).results().tabs().first() {
            Some(ninesql_grid::ResultTab::Table(m)) => m.borrow().row_count(),
            _ => 0,
        }
    })
}

#[gpui::test]
fn a_paged_table_saves_after_stop_fetching_and_keeps_its_edits(cx: &mut TestAppContext) {
    use ninesql_grid::FetchState;

    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let mut l = live(cx, ConnectionTypeId::dev(), tmp.path());
    // A table larger than one page: the read pauses after its first page, its run stays open.
    open_with_text(&mut l, "SELECT * FROM numbers(1000000)");
    let tab = l.tab.clone();
    wait_until(&mut l.vcx, |vcx| {
        tab.read_with(vcx, |t, cx| {
            t.table_state().describing.is_none()
                && matches!(
                    t.results().read(cx).results().tabs().first(),
                    Some(ninesql_grid::ResultTab::Table(m)) if m.borrow().state() == FetchState::Idle
                )
        })
    });
    assert!(
        tab.read_with(&l.vcx, |t, _| t.is_running()),
        "paused, not ended"
    );
    l.vcx
        .update(|_, cx| tab.update(cx, |t, cx| t.set_description(Ok(keyed_table()), cx)));
    assert!(tab.read_with(&l.vcx, |t, _| t.editing().is_some()));
    rename_first_row(&mut l);
    let loaded = loaded_rows(&l);
    assert!(loaded > 0 && loaded < 1_000_000, "{loaded}");

    // Refused while paging, with the way out named.
    l.vcx
        .update(|window, cx| tab.update(cx, |t, cx| t.save_changes(window, cx)));
    let label = tab.read_with(&l.vcx, |t, _| t.cancel_label());
    let message = tab
        .read_with(&l.vcx, |t, _| t.message().map(|m| m.text.to_string()))
        .unwrap_or_default();
    assert_eq!(message, super::table::save_blocked_by_run_text(label));
    assert!(message.contains(&format!("\"{label}\"")), "{message}");
    assert!(tab.read_with(&l.vcx, |t, _| t.has_pending_edits()));
    assert!(!tab.read_with(&l.vcx, |t, _| t.table_state().saving));

    // "Stop fetching" / "Cancel": the run ends, the rows and the pending edit stay.
    l.vcx
        .update(|window, cx| tab.update(cx, |t, cx| t.cancel_run(window, cx)));
    wait_until(&mut l.vcx, |vcx| !tab.read_with(vcx, |t, _| t.is_running()));
    assert_eq!(loaded_rows(&l), loaded, "the loaded rows are kept");
    assert!(tab.read_with(&l.vcx, |t, _| t.has_pending_edits()));
    assert!(tab.read_with(&l.vcx, |t, _| t.editing().is_some()));

    // Now the save goes through.
    l.vcx
        .update(|window, cx| tab.update(cx, |t, cx| t.save_changes(window, cx)));
    wait_until(&mut l.vcx, |vcx| {
        tab.read_with(vcx, |t, _| {
            !t.table_state().saving && !t.has_pending_edits()
        })
    });
    let message = tab
        .read_with(&l.vcx, |t, _| t.message().map(|m| m.text.to_string()))
        .unwrap_or_default();
    assert!(message.starts_with("Saved 1 changes."), "{message}");
    // The refresh after the save pages again: end it.
    l.vcx
        .update(|window, cx| tab.update(cx, |t, cx| t.cancel_run(window, cx)));
    wait_until(&mut l.vcx, |vcx| !tab.read_with(vcx, |t, _| t.is_running()));
}

#[gpui::test]
fn result_not_matching_the_query_provenance_is_read_only(cx: &mut TestAppContext) {
    // An aliased column: the result's names are not the table's, whatever the query said.
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let mut l = live(cx, ConnectionTypeId::dev(), tmp.path());
    open_with_text(&mut l, "SELECT 1 AS id");
    let tab = l.tab.clone();
    wait_until(&mut l.vcx, |vcx| {
        tab.read_with(vcx, |t, cx| {
            !t.is_running()
                && t.table_state().describing.is_none()
                && t.results().read(cx).tab_of_statement(0).is_some()
        })
    });
    l.vcx
        .update(|_, cx| tab.update(cx, |t, cx| t.set_description(Ok(keyed_table()), cx)));
    assert!(l.tab.read_with(&l.vcx, |t, _| t.editing().is_none()));
    let reason = l
        .tab
        .read_with(&l.vcx, |t, _| t.read_only_reason().map(str::to_owned))
        .unwrap_or_default();
    assert!(reason.starts_with("Read-only:"), "{reason}");
}

#[gpui::test]
fn typed_select_in_a_table_tab_is_never_editable(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let mut l = live(cx, ConnectionTypeId::dev(), tmp.path());
    open_editable(&mut l);
    // The user types an aliased SELECT of the same columns: no provenance, read-only.
    let editor = l.tab.read_with(&l.vcx, |t, _| t.editor().clone());
    l.vcx.update(|window, cx| {
        editor.update(cx, |e, cx| {
            e.set_text("SELECT * FROM numbers(3) AS t000", window, cx);
            e.execute(ExecutionKind::Statement, window, cx);
        });
    });
    let tab = l.tab.clone();
    wait_until(&mut l.vcx, |vcx| {
        tab.read_with(vcx, |t, cx| {
            !t.is_running() && t.results().read(cx).tab_of_statement(0).is_some()
        })
    });
    assert!(l.tab.read_with(&l.vcx, |t, _| t.editing().is_none()));
    assert_eq!(
        l.tab
            .read_with(&l.vcx, |t, _| t.read_only_reason().map(str::to_owned)),
        Some(TYPED_READ_ONLY.to_owned())
    );
}

#[test]
fn a_rolled_back_transaction_is_said_plainly() {
    use ninesql_core::EditError;
    let text = super::table::save_failed_text(&EditError::TransactionRolledBack {
        source: Box::new(EditError::Cancelled),
    });
    assert!(text.contains("whole transaction was rolled back"), "{text}");
    assert!(text.contains("earlier uncommitted changes"), "{text}");
    let text = super::table::save_failed_text(&EditError::UndoFailed {
        source: Box::new(EditError::Cancelled),
    });
    // The session is already closed (P5-E L2): say so, and connect again; no rollback offered.
    assert!(text.contains("closed the session"), "{text}");
    assert!(text.contains("Connect again"), "{text}");
    assert!(!text.contains("Roll back transaction"), "{text}");
    assert!(
        text.ends_with("Cause: cancelled; nothing was saved"),
        "{text}"
    );
    let text = super::table::save_failed_text(&EditError::TooManyChanges { max: 10_000 });
    assert!(text.contains("still pending"), "{text}");
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

#[test]
fn labels_mark_invisible_characters() {
    use ninesql_core::{ColumnFilter, FilterOp, SortDirection, SortKey};
    let p = ObjectPath::root()
        .child(ObjectKind::Schema, "pub\u{202E}lic")
        .child(ObjectKind::Table, "t\u{200B}1");
    assert_eq!(TableSource::of_path(p).label(), "pub\u{FFFD}lic.t\u{FFFD}1");
    assert_eq!(
        super::table::display_columns(&["a\u{2028}b".into(), "c".into()]),
        "a\u{FFFD}b, c"
    );
    assert_eq!(
        super::edit_dialogs::unchecked_columns_text(&["x\u{202E}".into()]),
        "Concurrent changes to these columns are not detected: x\u{FFFD}"
    );
    let filters = [ColumnFilter::new("na\u{202E}me", FilterOp::IsNull, None)];
    let order = [SortKey::new("i\u{200F}d", SortDirection::Descending)];
    assert_eq!(
        super::edit_dialogs::filter_label(&filters, None, &order),
        "na\u{FFFD}me is null · order by i\u{FFFD}d desc"
    );
}
