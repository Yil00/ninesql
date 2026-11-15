//! `TestAppContext` tests of the SQL editor tab against the core's mock driver: a statement's
//! rows reach the grid, a production `DELETE` without `WHERE` asks for confirmation and resumes
//! with the token, cancel, and the editor / grid actions in the keymap and the palette.

use gpui::{Entity, Focusable as _, TestAppContext, VisualTestContext};
use gpui_kit::component::WindowExt as _;
use ninesql_core::{Auth, ConnectionId, ConnectionTypeId};
use ninesql_editor::execution::ExecutionKind;
use ninesql_grid::ResultTab;
use ninesql_grid::execution::{ExecutionState, StatementStatus};

use super::SqlEditorTab;
use crate::connections::connect_flow::Purpose;
use crate::connections::tests::{mock_connection, mock_hub, open_live, wait_until};
use crate::workspace::Workspace;

/// A window on the mock driver with a connection of type `ty` open in the active editor.
fn connected(
    cx: &mut TestAppContext,
    ty: ConnectionTypeId,
    dir: &std::path::Path,
) -> (Entity<Workspace>, Entity<SqlEditorTab>, VisualTestContext) {
    let c = mock_connection("Live", ty, Auth::None);
    let id: ConnectionId = c.id;
    let hub = mock_hub(cx, dir, vec![c]);
    let (ws, mut vcx) = open_live(cx);
    ws.update_in(&mut vcx, |ws, window, cx| {
        ws.connect(id, Purpose::Open, window, cx);
    });
    wait_until(&mut vcx, |vcx| hub.read_with(vcx, |h, _| h.is_open(id)));
    let tab = ws.read_with(&vcx, |ws, _| ws.active_editor().cloned());
    let Some(tab) = tab else {
        panic!("an editor tab");
    };
    wait_until(&mut vcx, |vcx| {
        tab.read_with(vcx, |t, _| t.connection() == Some(id))
    });
    (ws, tab, vcx)
}

fn run(tab: &Entity<SqlEditorTab>, text: &str, kind: ExecutionKind, vcx: &mut VisualTestContext) {
    let editor = tab.read_with(vcx, |t, _| t.editor().clone());
    let text = text.to_owned();
    vcx.update(|window, cx| {
        editor.update(cx, |e, cx| {
            e.set_text(&text, window, cx);
            e.execute(kind, window, cx);
        });
    });
}

fn rows(tab: &Entity<SqlEditorTab>, vcx: &VisualTestContext) -> Vec<usize> {
    tab.read_with(vcx, |t, cx| {
        t.results()
            .read(cx)
            .results()
            .tabs()
            .iter()
            .filter_map(|tab| match tab {
                ResultTab::Table(m) => Some(m.borrow().row_count()),
                ResultTab::Documents { .. } => None,
            })
            .collect()
    })
}

fn update_counts(tab: &Entity<SqlEditorTab>, vcx: &VisualTestContext) -> Vec<u32> {
    tab.read_with(vcx, |t, cx| {
        t.results()
            .read(cx)
            .results()
            .update_counts()
            .iter()
            .map(|u| u.statement)
            .collect()
    })
}

#[gpui::test]
fn a_statement_shows_its_rows_in_the_grid(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let (_ws, tab, mut vcx) = connected(cx, ConnectionTypeId::dev(), tmp.path());
    run(
        &tab,
        "SELECT * FROM numbers(5)",
        ExecutionKind::Statement,
        &mut vcx,
    );
    wait_until(&mut vcx, |vcx| !tab.read_with(vcx, |t, _| t.is_running()));
    assert_eq!(rows(&tab, &vcx), vec![5]);
    let state = tab.read_with(&vcx, |t, cx| t.results().read(cx).results().state());
    assert_eq!(state, ExecutionState::Finished);
    let editor = tab.read_with(&vcx, |t, _| t.editor().clone());
    assert!(
        editor.read_with(&vcx, |e, _| e.running().is_none()),
        "finish_run reached the editor"
    );

    // A script: the error of the second statement is placed in the editor.
    run(
        &tab,
        "SELECT * FROM numbers(2);\nSELECT error();\nSELECT * FROM numbers(3)",
        ExecutionKind::Script,
        &mut vcx,
    );
    wait_until(&mut vcx, |vcx| !tab.read_with(vcx, |t, _| t.is_running()));
    let shown = editor.read_with(&vcx, |e, _| e.error().cloned());
    let Some(shown) = shown else {
        panic!("the failed statement is reported to the editor");
    };
    let Some(placed) = shown.placed else {
        panic!("placed on its statement");
    };
    assert_eq!(placed.position.line, 1, "the second line (0-based)");
}

#[gpui::test]
fn without_a_connection_nothing_runs_and_the_editor_says_why(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let _hub = mock_hub(cx, tmp.path(), Vec::new());
    let (ws, mut vcx) = open_live(cx);
    let Some(tab) = ws.read_with(&vcx, |ws, _| ws.active_editor().cloned()) else {
        panic!("an editor tab");
    };
    run(&tab, "SELECT 1 AS x", ExecutionKind::Statement, &mut vcx);
    vcx.run_until_parked();
    let editor = tab.read_with(&vcx, |t, _| t.editor().clone());
    let notice = editor.read_with(&vcx, |e, _| e.notice().cloned());
    assert!(notice.is_some_and(|n| n.contains("No open connection")));
    assert!(editor.read_with(&vcx, |e, _| e.running().is_none()));
}

#[gpui::test]
fn a_production_delete_without_where_is_confirmed_then_resumed(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let (ws, tab, mut vcx) = connected(cx, ConnectionTypeId::prod(), tmp.path());
    assert!(
        ws.read_with(&vcx, |ws, cx| ws.frame_color(cx).is_some()),
        "production frame from the editor's session"
    );
    run(
        &tab,
        "SELECT * FROM numbers(1);\nDELETE FROM t",
        ExecutionKind::Script,
        &mut vcx,
    );
    wait_until(&mut vcx, |vcx| {
        tab.read_with(vcx, |t, _| t.awaiting_confirmation())
    });
    assert_eq!(
        tab.read_with(&vcx, |t, _| t.pending_statement().map(str::to_owned)),
        Some("DELETE FROM t".to_owned())
    );
    assert!(vcx.update(|window, cx| window.has_active_dialog(cx)));
    // The first statement ran; the DELETE was not sent.
    assert!(update_counts(&tab, &vcx).is_empty());
    assert_eq!(rows(&tab, &vcx), vec![1]);

    // "Run" in the dialog.
    vcx.update(|window, cx| {
        window.close_dialog(cx);
        tab.update(cx, |t, cx| t.confirm(window, cx));
    });
    wait_until(&mut vcx, |vcx| !tab.read_with(vcx, |t, _| t.is_running()));
    assert_eq!(
        update_counts(&tab, &vcx),
        vec![1],
        "the DELETE ran once confirmed, numbered after the first statement"
    );
    assert_eq!(
        rows(&tab, &vcx),
        vec![1],
        "the first statement's rows are kept"
    );
    let statements = tab.read_with(&vcx, |t, cx| {
        t.results().read(cx).results().statements().clone()
    });
    assert!(
        statements
            .values()
            .all(|s| matches!(s, StatementStatus::Finished | StatementStatus::Closed)),
        "{statements:?}"
    );

    // "Cancel" stops the run before the statement.
    run(&tab, "DELETE FROM t", ExecutionKind::Statement, &mut vcx);
    wait_until(&mut vcx, |vcx| {
        tab.read_with(vcx, |t, _| t.awaiting_confirmation())
    });
    vcx.update(|window, cx| {
        window.close_dialog(cx);
        tab.update(cx, |t, cx| t.decline(window, cx));
    });
    vcx.run_until_parked();
    assert!(!tab.read_with(&vcx, |t, _| t.is_running()));
    assert!(update_counts(&tab, &vcx).is_empty(), "declined: not run");
    let message = tab.read_with(&vcx, |t, _| t.message().map(|m| m.text.to_string()));
    assert!(message.is_some_and(|m| m.contains("not confirmed")));
}

#[gpui::test]
fn cancel_stops_the_running_statement(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let (_ws, tab, mut vcx) = connected(cx, ConnectionTypeId::dev(), tmp.path());
    run(
        &tab,
        "SELECT * FROM slow_numbers(1000000, 50)",
        ExecutionKind::Statement,
        &mut vcx,
    );
    wait_until(&mut vcx, |vcx| {
        tab.read_with(vcx, |t, cx| {
            t.results().read(cx).results().state() == ExecutionState::Running
        })
    });
    // The editor's "Cancel execution" (ctrl-.) goes through the sink to `RunControl::cancel`.
    let editor = tab.read_with(&vcx, |t, _| t.editor().clone());
    vcx.update(|window, cx| {
        let focus = editor.read(cx).focus_handle(cx);
        focus.focus(window, cx);
    });
    vcx.simulate_keystrokes("ctrl-.");
    wait_until(&mut vcx, |vcx| !tab.read_with(vcx, |t, _| t.is_running()));
    assert!(editor.read_with(&vcx, |e, _| e.running().is_none()));
    let state = tab.read_with(&vcx, |t, cx| t.results().read(cx).results().state());
    assert_ne!(state, ExecutionState::Running, "{state:?}");
    let message = tab.read_with(&vcx, |t, _| t.message().map(|m| m.text.to_string()));
    assert_eq!(message.as_deref(), Some("Cancelled."));
}

#[gpui::test]
fn editor_and_grid_actions_are_in_the_keymap_and_the_palette(cx: &mut TestAppContext) {
    let (settings, diagnostics) =
        crate::settings::parse("[keybindings]\n\"ninesql_editor::ExecuteStatement\" = \"f5\"\n");
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    cx.update(|cx| {
        crate::app::init(
            crate::settings::SettingsState {
                path: None,
                settings,
                diagnostics,
                save_blocked: None,
            },
            cx,
        );
    });
    let keymap = cx.read_global::<crate::keymap::EffectiveKeymap, _>(|k, _| k.clone());
    assert_eq!(keymap.keys_for("ninesql_editor::ExecuteStatement"), ["f5"]);
    assert_eq!(keymap.keys_for("ninesql_grid::CopySelection"), ["ctrl-c"]);
    cx.update(|cx| {
        let bindings = cx.key_bindings();
        let bindings = bindings.borrow();
        let execute = ninesql_editor::actions::ExecuteStatement;
        assert_eq!(
            bindings.bindings_for_action(&execute).count(),
            1,
            "the override replaced ctrl-enter"
        );
        let copy = ninesql_grid::actions::CopySelection;
        assert_eq!(bindings.bindings_for_action(&copy).count(), 1);
    });
    let groups = crate::palette::grouped_entries(&keymap);
    let names: Vec<&str> = groups
        .iter()
        .flat_map(|(_, e)| e.iter().map(|e| e.spec.name()))
        .collect();
    for spec in ninesql_editor::EDITOR_ACTIONS {
        assert!(names.contains(&spec.name()), "{}", spec.name());
    }
    for spec in ninesql_grid::actions::GRID_ACTIONS {
        assert!(names.contains(&spec.name()), "{}", spec.name());
    }

    // The palette runs an editor action in the active editor (no connection: it says so).
    let Some((window, ws)) = cx.update(crate::app::open_window) else {
        panic!("a window");
    };
    let mut vcx = VisualTestContext::from_window(window, cx);
    vcx.run_until_parked();
    vcx.simulate_keystrokes("ctrl-shift-p");
    vcx.simulate_input("execute statement");
    vcx.simulate_keystrokes("enter");
    vcx.run_until_parked();
    let Some(tab) = ws.read_with(&vcx, |ws, _| ws.active_editor().cloned()) else {
        panic!("an editor tab");
    };
    let editor = tab.read_with(&vcx, |t, _| t.editor().clone());
    let notice = editor.read_with(&vcx, |e, _| e.notice().cloned());
    assert!(notice.is_some(), "the editor handled the palette's action");
}
