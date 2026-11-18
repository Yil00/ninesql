//! `TestAppContext` tests of the export / import dialogs against the core's mock driver: a new
//! file, the explicit overwrite step, a summary; the CSV preview, the mapping editor, the
//! production confirmation shown exactly and resumed with its token; "Format SQL" and
//! "Export results…" from an editor tab.

use gpui::{Entity, TestAppContext, VisualTestContext};
use ninesql_core::transfer::TargetColumn;
use ninesql_core::{Auth, ConnectionId, ConnectionTypeId, ImportEnd, SessionHandle};
use ninesql_editor::execution::ExecutionKind;

use super::export::{ExportDialog, ExportStage, ExportTarget, open_export_dialog};
use super::import::{ImportCsvDialog, ImportStage, open_import_dialog};
use crate::connections::connect_flow::Purpose;
use crate::connections::hub::ConnectionHub;
use crate::connections::tests::{mock_connection, mock_hub_with_transfer, open_live, wait_until};
use crate::editor_tab::SqlEditorTab;

struct Live {
    hub: Entity<ConnectionHub>,
    tab: Entity<SqlEditorTab>,
    session: SessionHandle,
    vcx: VisualTestContext,
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
        hub,
        tab,
        session,
        vcx,
    }
}

fn set_path<V: 'static>(
    input: impl Fn(&V) -> Entity<gpui_kit::component::input::InputState>,
    dialog: &Entity<V>,
    path: &std::path::Path,
    vcx: &mut VisualTestContext,
) {
    let input = dialog.read_with(vcx, |d, _| input(d));
    let text = path.to_string_lossy().into_owned();
    vcx.update(|window, cx| input.update(cx, |s, cx| s.set_value(text, window, cx)));
}

fn export_dialog(l: &mut Live, target: ExportTarget) -> Entity<ExportDialog> {
    let transfer = l.hub.read_with(&l.vcx, |h, _| h.transfer());
    let Some(transfer) = transfer else {
        panic!("a transfer service");
    };
    let session = l.session.clone();
    l.vcx
        .update(|window, cx| open_export_dialog(session, transfer, target, window, cx))
}

#[gpui::test]
fn export_writes_a_new_file_and_asks_before_replacing(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let out = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let mut l = live(cx, ConnectionTypeId::dev(), tmp.path());
    let dialog = export_dialog(
        &mut l,
        ExportTarget::Statement {
            text: "SELECT * FROM numbers(5)".into(),
        },
    );
    let file = out.path().join("rows.csv");
    // Without a file, "Export" says why.
    assert_eq!(
        dialog.read_with(&l.vcx, |d, cx| d.start_problem(cx)),
        Some("Choose the file to write.")
    );
    set_path(
        |d: &ExportDialog| d.path_input().clone(),
        &dialog,
        &file,
        &mut l.vcx,
    );
    dialog.update_in(&mut l.vcx, |d, window, cx| d.start(window, cx));
    wait_until(&mut l.vcx, |vcx| dialog.read_with(vcx, |d, _| !d.is_busy()));
    let lines = dialog.read_with(&l.vcx, |d, _| match d.stage() {
        ExportStage::Done(s) => ExportDialog::summary_lines(s),
        other => panic!("not done: {other:?}"),
    });
    assert!(lines[0].starts_with("5 rows"), "{lines:?}");
    let text = std::fs::read_to_string(&file).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(text.lines().count(), 6, "header and 5 rows");
    // Again: the file exists, replacing it is its own step.
    dialog.update_in(&mut l.vcx, |d, window, cx| d.start(window, cx));
    wait_until(&mut l.vcx, |vcx| {
        dialog.read_with(vcx, |d, _| {
            matches!(d.stage(), ExportStage::ConfirmOverwrite(_))
        })
    });
    dialog.update(&mut l.vcx, |d, cx| d.back(cx));
    assert!(dialog.read_with(&l.vcx, |d, _| matches!(d.stage(), ExportStage::Editing)));
    dialog.update_in(&mut l.vcx, |d, window, cx| d.start(window, cx));
    wait_until(&mut l.vcx, |vcx| {
        dialog.read_with(vcx, |d, _| {
            matches!(d.stage(), ExportStage::ConfirmOverwrite(_))
        })
    });
    dialog.update_in(&mut l.vcx, |d, window, cx| d.confirm_overwrite(window, cx));
    wait_until(&mut l.vcx, |vcx| {
        dialog.read_with(vcx, |d, _| matches!(d.stage(), ExportStage::Done(_)))
    });
}

#[gpui::test]
fn export_of_a_write_is_refused_by_the_core(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let out = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let mut l = live(cx, ConnectionTypeId::dev(), tmp.path());
    let dialog = export_dialog(
        &mut l,
        ExportTarget::Statement {
            text: "DELETE FROM t".into(),
        },
    );
    let file = out.path().join("x.csv");
    set_path(
        |d: &ExportDialog| d.path_input().clone(),
        &dialog,
        &file,
        &mut l.vcx,
    );
    dialog.update_in(&mut l.vcx, |d, window, cx| d.start(window, cx));
    wait_until(&mut l.vcx, |vcx| {
        dialog.read_with(vcx, |d, _| {
            !d.is_busy() && !matches!(d.stage(), ExportStage::Editing)
        })
    });
    let failed = dialog.read_with(&l.vcx, |d, _| match d.stage() {
        ExportStage::Failed(why) => why.clone(),
        other => panic!("not refused: {other:?}"),
    });
    assert!(failed.contains("only a query that reads data"), "{failed}");
    assert!(!file.exists());
}

#[gpui::test]
fn import_previews_maps_and_confirms_on_production(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let data = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let mut l = live(cx, ConnectionTypeId::prod(), tmp.path());
    let file = data.path().join("people.csv");
    std::fs::write(&file, "id,label\n1,a\n2,b\nx,c\n").unwrap_or_else(|e| panic!("{e}"));
    let targets = vec![
        TargetColumn::new("id", "integer"),
        TargetColumn::new("label", "text"),
    ];
    let transfer = l.hub.read_with(&l.vcx, |h, _| h.transfer());
    let Some(transfer) = transfer else {
        panic!("a transfer service");
    };
    let session = l.session.clone();
    let dialog: Entity<ImportCsvDialog> = l.vcx.update(|window, cx| {
        open_import_dialog(
            session,
            transfer,
            vec!["people".into()],
            None,
            targets,
            window,
            cx,
        )
    });
    set_path(
        |d: &ImportCsvDialog| d.path_input().clone(),
        &dialog,
        &file,
        &mut l.vcx,
    );
    dialog.update_in(&mut l.vcx, |d, window, cx| d.read_preview(window, cx));
    wait_until(&mut l.vcx, |vcx| {
        dialog.read_with(vcx, |d, _| matches!(d.stage(), ImportStage::Preview))
    });
    let (mapping, issues) = dialog.read_with(&l.vcx, |d, _| {
        (
            d.mapping().to_vec(),
            d.preview().map_or(0, |p| p.issues_total),
        )
    });
    assert_eq!(mapping, vec![Some(0), Some(1)], "by header name");
    assert_eq!(issues, 1, "x is not an integer");
    // The mapping editor cycles; a field mapped elsewhere moves.
    dialog.update(&mut l.vcx, |d, cx| d.cycle_mapping(0, cx));
    assert_eq!(
        dialog.read_with(&l.vcx, |d, _| d.mapping().to_vec()),
        vec![Some(1), None]
    );
    dialog.update(&mut l.vcx, |d, cx| d.map(0, Some(0), cx));
    dialog.update(&mut l.vcx, |d, cx| d.map(1, Some(1), cx));
    dialog.update(&mut l.vcx, |d, cx| d.set_skip_invalid(true, cx));
    // Production: the core's confirmation, shown exactly.
    dialog.update_in(&mut l.vcx, |d, window, cx| d.start(window, cx));
    wait_until(&mut l.vcx, |vcx| {
        dialog.read_with(vcx, |d, _| matches!(d.stage(), ImportStage::Confirm(_)))
    });
    let lines = dialog.read_with(&l.vcx, |d, _| match d.stage() {
        ImportStage::Confirm(c) => ImportCsvDialog::confirmation_lines(c),
        _ => Vec::new(),
    });
    assert_eq!(lines[0], ("Connection".into(), "Live (production)".into()));
    assert_eq!(lines[2], ("Columns".into(), "id, label".into()));
    assert_eq!(lines[3].0, "File");
    // The commit mode, and what "Roll back" cannot undo on a manual-commit session (M4).
    assert_eq!(
        lines[4],
        (
            "Commits".into(),
            "All or nothing: one transaction, committed at the end".into()
        )
    );
    let manual = !l.session.transaction_state().auto_commit;
    let mut expected = 5;
    if manual {
        assert_eq!(
            lines[5],
            (
                "Note".into(),
                "Rows are committed by the import (at the end); Roll back cannot undo them.".into()
            )
        );
        expected += 1;
    }
    assert_eq!(
        lines.len(),
        expected,
        "the mock table rolls back: {lines:?}"
    );
    // The core's answer about the table came back off the UI thread: all or nothing allowed.
    assert!(dialog.read_with(&l.vcx, |d, _| {
        d.target().is_some_and(|t| t.all_or_nothing_allowed())
    }));
    // The statement is shown whole in the script view, never cut.
    let script = dialog
        .read_with(&l.vcx, |d, _| d.confirmation_script())
        .unwrap_or_else(|| panic!("statement shown"));
    assert!(
        script.shown_text().starts_with("INSERT INTO \"people\""),
        "{}",
        script.shown_text()
    );
    // The 720 px dialog measured its row box and wrapped the rows to it (phase 5 re-review
    // M-A): the widest row fits the measured width.
    l.vcx.run_until_parked();
    l.vcx.update(|window, cx| window.simulate_next_frame(cx));
    l.vcx.run_until_parked();
    let (content, advance) = script
        .measured()
        .unwrap_or_else(|| panic!("the script box was measured"));
    assert!(content > 300. && content < 720., "{content}");
    // The test text system advances 0.6 em: the canvas sees the rows' 12 px text style.
    assert!((advance - 7.2).abs() < 0.01, "{advance}");
    let width =
        crate::script_view::WrapWidth::measured(content, advance, 12., script.gutter_chars(), 8.);
    assert_eq!(script.width(), width);
    let widest = script.row_columns(script.widest_row()).unwrap_or(0);
    let gutter = crate::script_view::WrapWidth::gutter_px(advance, 12., script.gutter_chars());
    #[allow(clippy::cast_precision_loss)]
    let needed = gutter + 8. + widest as f32 * advance;
    assert!(needed <= content, "{needed} of {content}");
    // Cancel goes back to the preview; nothing ran.
    dialog.update(&mut l.vcx, |d, cx| d.decline(cx));
    assert!(dialog.read_with(&l.vcx, |d, _| matches!(d.stage(), ImportStage::Preview)));
    dialog.update_in(&mut l.vcx, |d, window, cx| d.start(window, cx));
    wait_until(&mut l.vcx, |vcx| {
        dialog.read_with(vcx, |d, _| matches!(d.stage(), ImportStage::Confirm(_)))
    });
    dialog.update_in(&mut l.vcx, |d, window, cx| d.confirm(window, cx));
    wait_until(&mut l.vcx, |vcx| {
        dialog.read_with(vcx, |d, _| matches!(d.stage(), ImportStage::Done(_)))
    });
    let (end, inserted, skipped) = dialog.read_with(&l.vcx, |d, _| match d.stage() {
        ImportStage::Done(s) => (s.end, s.rows_inserted, s.rows_skipped),
        _ => panic!("done"),
    });
    assert_eq!(end, ImportEnd::Completed);
    assert_eq!((inserted, skipped), (2, 1));
}

#[gpui::test]
fn export_results_and_format_sql_from_the_editor(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let mut l = live(cx, ConnectionTypeId::dev(), tmp.path());
    // Nothing ran yet.
    assert!(
        l.tab
            .read_with(&l.vcx, |t, cx| t.export_statement(cx))
            .is_err()
    );
    let editor = l.tab.read_with(&l.vcx, |t, _| t.editor().clone());
    l.vcx.update(|window, cx| {
        editor.update(cx, |e, cx| {
            e.set_text("select * from numbers(3)", window, cx);
            e.execute(ExecutionKind::Statement, window, cx);
        });
    });
    let tab = l.tab.clone();
    wait_until(&mut l.vcx, |vcx| !tab.read_with(vcx, |t, _| t.is_running()));
    assert_eq!(
        l.tab.read_with(&l.vcx, |t, cx| t.export_statement(cx)),
        Ok("select * from numbers(3)".to_owned())
    );
    let changed = l
        .tab
        .update_in(&mut l.vcx, |t, window, cx| t.format_sql(window, cx));
    assert!(changed);
    let text = editor.read_with(&l.vcx, |e, cx| e.text(cx));
    assert!(text.starts_with("SELECT"), "{text}");
}

#[test]
fn table_parts_follow_the_core_naming() {
    use ninesql_core::driver_api::ObjectKind;
    use ninesql_core::driver_api::metadata::ObjectPath;
    let p = ObjectPath::root()
        .child(ObjectKind::Catalog, "app")
        .child(ObjectKind::Schema, "public")
        .child(ObjectKind::Table, "users");
    assert_eq!(super::table_parts(&p), vec!["app", "public", "users"]);
    assert_eq!(
        super::table_parts(&p),
        ninesql_core::TableQuery::of_path(&p).table,
        "the same object as editing"
    );
    let p = ObjectPath::root()
        .child(ObjectKind::Schema, "main")
        .child(ObjectKind::Table, "t");
    assert_eq!(super::table_parts(&p), vec!["main", "t"]);
    let p = ObjectPath::root()
        .child(ObjectKind::Catalog, "shop")
        .child(ObjectKind::Table, "t");
    assert_eq!(super::table_parts(&p), vec!["shop", "t"]);
    let p = ObjectPath::root().child(ObjectKind::Table, "t");
    assert_eq!(super::table_parts(&p), vec!["t"]);
}

#[gpui::test]
fn navigator_transfers_avoid_the_metadata_session_and_busy_ones(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let c = mock_connection("Live", ConnectionTypeId::dev(), Auth::None);
    let id: ConnectionId = c.id;
    let hub = mock_hub_with_transfer(cx, tmp.path(), vec![c]);
    let (ws, mut vcx) = open_live(cx);
    let pick = |ws: &Entity<crate::workspace::Workspace>, vcx: &VisualTestContext| {
        ws.read_with(vcx, |ws, cx| ws.transfer_session(id, cx).map(|s| s.id()))
    };
    assert_eq!(pick(&ws, &vcx), None, "no session: none");

    for n in 1..=2 {
        ws.update_in(&mut vcx, |ws, window, cx| {
            ws.connect(id, Purpose::Open, window, cx);
        });
        wait_until(&mut vcx, |vcx| {
            hub.read_with(vcx, |h, _| h.sessions(id).len() == n)
                && !ws.read_with(vcx, |ws, cx| ws.flow().read(cx).is_busy())
        });
    }
    let (oldest, newest) = hub.read_with(&vcx, |h, _| {
        let s = h.sessions(id);
        (s[0].clone(), s[1].clone())
    });
    let tab = ws
        .read_with(&vcx, |ws, _| ws.active_editor().cloned())
        .unwrap_or_else(|| panic!("an editor tab"));
    // The active editor took the first session: that one, the user's choice.
    assert_eq!(
        tab.read_with(&vcx, |t, _| t.session().map(|s| s.id())),
        Some(oldest.id())
    );
    assert_eq!(pick(&ws, &vcx), Some(oldest.id()));

    // An editor with no session: the newest, never the metadata service's oldest one.
    tab.update(&mut vcx, |t, cx| t.set_session(None, cx));
    assert_eq!(pick(&ws, &vcx), Some(newest.id()));

    // An editor paging a result on the newest: that session is busy, the oldest is free.
    tab.update(&mut vcx, |t, cx| t.set_session(Some(newest.clone()), cx));
    let editor = tab.read_with(&vcx, |t, _| t.editor().clone());
    vcx.update(|window, cx| {
        editor.update(cx, |e, cx| {
            e.set_text("SELECT * FROM numbers(1000000)", window, cx);
            e.execute(ExecutionKind::Statement, window, cx);
        });
    });
    wait_until(&mut vcx, |vcx| tab.read_with(vcx, |t, _| t.is_running()));
    assert_eq!(pick(&ws, &vcx), Some(oldest.id()));
    tab.update_in(&mut vcx, |t, window, cx| t.cancel_run(window, cx));
    wait_until(&mut vcx, |vcx| !tab.read_with(vcx, |t, _| t.is_running()));
    assert_eq!(pick(&ws, &vcx), Some(newest.id()));
}

#[gpui::test]
fn import_without_transactions_offers_only_commit_every_n_rows(cx: &mut TestAppContext) {
    use ninesql_core::ImportTransaction;
    use ninesql_core::driver_api::mock::{MockDriver, full_capabilities};

    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let mut caps = full_capabilities();
    caps.transactions.manual_commit = false;
    caps.transactions.savepoints = false;
    let c = mock_connection("NoTx", ConnectionTypeId::dev(), Auth::None);
    let id: ConnectionId = c.id;
    let hub = crate::connections::tests::mock_hub_with_transfer_driver(
        cx,
        tmp.path(),
        vec![c],
        MockDriver::new().with_capabilities(caps),
    );
    let (ws, mut vcx) = open_live(cx);
    ws.update_in(&mut vcx, |ws, window, cx| {
        ws.connect(id, Purpose::Open, window, cx);
    });
    wait_until(&mut vcx, |vcx| hub.read_with(vcx, |h, _| h.is_open(id)));
    let session = hub
        .read_with(&vcx, |h, _| h.sessions(id).first().cloned())
        .unwrap_or_else(|| panic!("a session"));
    let transfer = hub
        .read_with(&vcx, |h, _| h.transfer())
        .unwrap_or_else(|| panic!("a transfer service"));
    let path = ninesql_core::driver_api::metadata::ObjectPath::root()
        .child(ninesql_core::driver_api::ObjectKind::Catalog, "nine")
        .child(ninesql_core::driver_api::ObjectKind::Schema, "public")
        .child(ninesql_core::driver_api::ObjectKind::Table, "t000");
    let parts = super::table_parts(&path);
    let dialog: Entity<ImportCsvDialog> = vcx.update(|window, cx| {
        open_import_dialog(
            session,
            transfer,
            parts,
            Some(path),
            vec![TargetColumn::new("id", "bigint")],
            window,
            cx,
        )
    });
    wait_until(&mut vcx, |vcx| {
        dialog.read_with(vcx, |d, _| d.target().is_some())
    });
    let (mode, refusal) =
        dialog.read_with(&vcx, |d, _| (d.transaction(), d.all_or_nothing_refusal()));
    assert_eq!(
        mode,
        ImportTransaction::CommitEvery(super::import::COMMIT_EVERY_ROWS)
    );
    let refusal = refusal.unwrap_or_else(|| panic!("refused"));
    assert!(refusal.contains("no transactions"), "{refusal}");
    // Choosing "All or nothing" is refused with the reason; the mode stays.
    dialog.update(&mut vcx, |d, cx| {
        d.set_transaction(ImportTransaction::AllOrNothing, cx);
    });
    let (mode, stage) = dialog.read_with(&vcx, |d, _| {
        (
            d.transaction(),
            match d.stage() {
                ImportStage::Failed(why) => why.clone(),
                _ => String::new(),
            },
        )
    });
    assert_eq!(
        mode,
        ImportTransaction::CommitEvery(super::import::COMMIT_EVERY_ROWS)
    );
    assert_eq!(stage, refusal);
}

#[test]
fn new_transfer_errors_and_the_tidb_warning_have_texts() {
    use ninesql_core::{ExportWarning, TransferJobError};
    let w = super::export::warning_text(ExportWarning::NoReadOnlyTransaction);
    assert!(w.contains("no engine-side read-only transaction"), "{w}");
    assert_ne!(w, "The export reported a warning.");
    for (e, needle) in [
        (TransferJobError::NonTransactionalTable, "cannot roll back"),
        (TransferJobError::TableStorageUnknown, "could not be read"),
        (
            TransferJobError::NoReadOnlyTransaction,
            "read-only connection",
        ),
    ] {
        let text = super::error_text(&e);
        assert!(text.contains(needle), "{text}");
        assert!(text.contains("commit every N rows") || needle == "read-only connection");
    }
}
