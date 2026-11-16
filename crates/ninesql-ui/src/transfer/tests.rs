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
    assert!(
        lines[3].1.starts_with("INSERT INTO \"people\""),
        "{lines:?}"
    );
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
fn table_parts_follow_the_nearest_schema() {
    use ninesql_core::driver_api::ObjectKind;
    use ninesql_core::driver_api::metadata::ObjectPath;
    let p = ObjectPath::root()
        .child(ObjectKind::Catalog, "app")
        .child(ObjectKind::Schema, "public")
        .child(ObjectKind::Table, "users");
    assert_eq!(super::table_parts(&p), vec!["public", "users"]);
    let p = ObjectPath::root().child(ObjectKind::Table, "t");
    assert_eq!(super::table_parts(&p), vec!["t"]);
}
