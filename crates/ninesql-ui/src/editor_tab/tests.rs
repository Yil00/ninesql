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

/// A window on the mock driver with `connections` saved; the first one is opened in the active
/// editor.
fn connected_with(
    cx: &mut TestAppContext,
    connections: Vec<ninesql_core::Connection>,
    dir: &std::path::Path,
) -> (
    Entity<Workspace>,
    Entity<crate::connections::hub::ConnectionHub>,
    Entity<SqlEditorTab>,
    VisualTestContext,
) {
    let id = connections[0].id;
    let hub = mock_hub(cx, dir, connections);
    let (ws, mut vcx) = open_live(cx);
    ws.update_in(&mut vcx, |ws, window, cx| {
        ws.connect(id, Purpose::Open, window, cx);
    });
    wait_until(&mut vcx, |vcx| hub.read_with(vcx, |h, _| h.is_open(id)));
    let Some(tab) = ws.read_with(&vcx, |ws, _| ws.active_editor().cloned()) else {
        panic!("an editor tab");
    };
    wait_until(&mut vcx, |vcx| {
        tab.read_with(vcx, |t, _| t.connection() == Some(id))
    });
    (ws, hub, tab, vcx)
}

fn production_color() -> gpui::Hsla {
    crate::connections::visuals::hsla(ninesql_core::ConnectionType::production().color)
}

fn message(tab: &Entity<SqlEditorTab>, vcx: &VisualTestContext) -> String {
    tab.read_with(vcx, |t, _| {
        t.message().map(|m| m.text.to_string()).unwrap_or_default()
    })
}

fn in_transaction(tab: &Entity<SqlEditorTab>, vcx: &VisualTestContext) -> bool {
    tab.read_with(vcx, |t, _| {
        t.session()
            .is_some_and(|s| s.transaction_state().in_transaction)
    })
}

#[gpui::test]
fn a_confirmed_statement_runs_only_on_the_session_the_run_started_on(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let live = mock_connection("Live", ConnectionTypeId::prod(), Auth::None);
    let other = mock_connection("Other", ConnectionTypeId::dev(), Auth::None);
    let (live_id, other_id) = (live.id, other.id);
    let (ws, hub, tab, mut vcx) = connected_with(cx, vec![live, other], tmp.path());
    let Some(live_session) = tab.read_with(&vcx, |t, _| t.session().map(|s| s.id())) else {
        panic!("a session");
    };
    run(
        &tab,
        "SELECT * FROM numbers(1);\nDELETE FROM t",
        ExecutionKind::Script,
        &mut vcx,
    );
    wait_until(&mut vcx, |vcx| {
        tab.read_with(vcx, |t, _| t.awaiting_confirmation())
    });
    // The dialog shows the run's session: production, in the production color.
    let badges = tab.read_with(&vcx, |t, cx| t.run_badges(cx));
    let Some(badges) = badges else {
        panic!("badges of the run's session");
    };
    assert!(badges.production);
    assert_eq!(badges.color, production_color());

    // Another connection opens meanwhile: the editor keeps its session.
    ws.update_in(&mut vcx, |ws, window, cx| {
        ws.connect(other_id, Purpose::Open, window, cx);
    });
    wait_until(&mut vcx, |vcx| {
        hub.read_with(vcx, |h, _| h.is_open(other_id))
    });
    let Some(other_session) = hub.read_with(&vcx, |h, _| h.sessions(other_id).last().cloned())
    else {
        panic!("the other session");
    };
    // The picker cannot move a running editor to another session.
    let changed = tab.update(&mut vcx, |t, cx| t.set_session(Some(other_session), cx));
    assert!(!changed, "refused during a run");
    assert!(message(&tab, &vcx).contains("run is going on"));
    let none = tab.update(&mut vcx, |t, cx| t.set_session(None, cx));
    assert!(!none);
    assert_eq!(
        tab.read_with(&vcx, |t, _| t.session().map(|s| s.id())),
        Some(live_session)
    );

    // The run's session closes while the dialog is up, and a background connect finishes:
    // the running editor is not given the new session.
    hub.update(&mut vcx, |h, cx| h.disconnect(live_id, cx));
    wait_until(&mut vcx, |vcx| {
        tab.read_with(vcx, |t, _| t.session().is_none())
    });
    ws.update_in(&mut vcx, |ws, window, cx| {
        ws.connect(other_id, Purpose::Open, window, cx);
    });
    wait_until(&mut vcx, |vcx| {
        hub.read_with(vcx, |h, _| h.sessions(other_id).len() == 2)
    });
    vcx.run_until_parked();
    assert!(tab.read_with(&vcx, |t, _| t.session().is_none()));
    assert_eq!(
        tab.read_with(&vcx, |t, _| t.run_session().map(|s| s.id())),
        Some(live_session)
    );

    // "Run" now ends the run: nothing goes to another session.
    vcx.update(|window, cx| {
        window.close_dialog(cx);
        tab.update(cx, |t, cx| t.confirm(window, cx));
    });
    vcx.run_until_parked();
    assert!(!tab.read_with(&vcx, |t, _| t.is_running()));
    assert!(
        update_counts(&tab, &vcx).is_empty(),
        "the DELETE was not run"
    );
    assert!(message(&tab, &vcx).contains("closed"));
    let other_counts = hub.read_with(&vcx, |h, _| {
        h.sessions(other_id)
            .iter()
            .filter(|s| s.transaction_state().in_transaction)
            .count()
    });
    assert_eq!(other_counts, 0);
}

#[gpui::test]
fn a_production_session_without_its_saved_connection_keeps_badge_and_frames(
    cx: &mut TestAppContext,
) {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let live = mock_connection("Live", ConnectionTypeId::prod(), Auth::None);
    let id = live.id;
    let (ws, hub, tab, mut vcx) = connected_with(cx, vec![live], tmp.path());
    hub.update(&mut vcx, |h, cx| h.forget_connection(id, cx));
    assert!(hub.read_with(&vcx, |h, _| h.connection(id).is_none()));
    assert_eq!(
        tab.read_with(&vcx, |t, cx| t.frame_color(cx)),
        Some(production_color())
    );
    assert!(ws.read_with(&vcx, |ws, cx| ws.frame_color(cx).is_some()));

    // Split views: each pane has its own production frame.
    ws.update_in(&mut vcx, |ws, window, cx| {
        ws.split(crate::layout::SplitAxis::Horizontal, window, cx);
    });
    let panes = ws.read_with(&vcx, |ws, _| ws.layout().pane_order());
    assert_eq!(panes.len(), 2);
    for pane in &panes {
        assert!(
            ws.read_with(&vcx, |ws, cx| ws.pane_frame_color(*pane, cx).is_some()),
            "both editors run on the production session"
        );
    }
    let Some(second) = ws.read_with(&vcx, |ws, _| ws.active_editor().cloned()) else {
        panic!("the new editor");
    };
    assert!(second.update(&mut vcx, |t, cx| t.set_session(None, cx)));
    let framed: Vec<bool> = panes
        .iter()
        .map(|p| ws.read_with(&vcx, |ws, cx| ws.pane_frame_color(*p, cx).is_some()))
        .collect();
    assert_eq!(framed, vec![true, false]);
}

#[gpui::test]
fn commit_waits_for_the_run_and_close_tab_waits_for_the_commit(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let live = mock_connection("Live", ConnectionTypeId::prod(), Auth::None);
    let (ws, _hub, tab, mut vcx) = connected_with(cx, vec![live], tmp.path());
    run(&tab, "BEGIN", ExecutionKind::Statement, &mut vcx);
    wait_until(&mut vcx, |vcx| !tab.read_with(vcx, |t, _| t.is_running()));
    assert!(in_transaction(&tab, &vcx));

    run(
        &tab,
        "SELECT * FROM slow_numbers(1000000, 50)",
        ExecutionKind::Statement,
        &mut vcx,
    );
    wait_until(&mut vcx, |vcx| tab.read_with(vcx, |t, _| t.is_running()));
    // The keyboard's Commit cannot land in the middle of a run.
    ws.update(&mut vcx, |ws, cx| ws.end_transaction(true, cx));
    vcx.run_until_parked();
    assert!(message(&tab, &vcx).contains("run is going on"));
    assert!(in_transaction(&tab, &vcx));

    // Closing the tab asks; "Commit and close" refused by the run keeps the tab.
    let Some(tab_id) = ws.read_with(&vcx, |ws, _| ws.layout().active_tab()) else {
        panic!("a tab");
    };
    ws.update_in(&mut vcx, |ws, window, cx| ws.close_tab(tab_id, window, cx));
    assert!(vcx.update(|window, cx| window.has_active_dialog(cx)));
    vcx.update(|window, cx| window.close_dialog(cx));
    ws.update_in(&mut vcx, |ws, window, cx| {
        ws.end_transaction_and_close_tab(tab_id, true, window, cx);
    });
    vcx.run_until_parked();
    assert!(
        ws.read_with(&vcx, |ws, _| ws.editor(tab_id).is_some()),
        "kept: the commit did not happen"
    );
    assert!(in_transaction(&tab, &vcx));

    // Once the run is over, the commit succeeds and only then the tab closes.
    tab.update_in(&mut vcx, |t, window, cx| t.cancel_run(window, cx));
    wait_until(&mut vcx, |vcx| !tab.read_with(vcx, |t, _| t.is_running()));
    let session = tab.read_with(&vcx, |t, _| t.session().cloned());
    let Some(session) = session else {
        panic!("a session");
    };
    ws.update_in(&mut vcx, |ws, window, cx| {
        ws.end_transaction_and_close_tab(tab_id, true, window, cx);
    });
    wait_until(&mut vcx, |vcx| {
        ws.read_with(vcx, |ws, _| ws.editor(tab_id).is_none())
    });
    assert!(!session.transaction_state().in_transaction, "committed");
}

#[gpui::test]
fn deleting_a_connection_asks_about_its_open_transaction(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let live = mock_connection("Live", ConnectionTypeId::prod(), Auth::None);
    let id = live.id;
    let (ws, hub, tab, mut vcx) = connected_with(cx, vec![live], tmp.path());
    run(&tab, "BEGIN", ExecutionKind::Statement, &mut vcx);
    wait_until(&mut vcx, |vcx| !tab.read_with(vcx, |t, _| t.is_running()));
    assert!(in_transaction(&tab, &vcx));
    let Some(session) = tab.read_with(&vcx, |t, _| t.session().cloned()) else {
        panic!("a session");
    };

    ws.update_in(&mut vcx, |ws, window, cx| {
        ws.delete_connection_now(id, window, cx);
    });
    vcx.run_until_parked();
    assert!(
        vcx.update(|window, cx| window.has_active_dialog(cx)),
        "asks first"
    );
    assert!(hub.read_with(&vcx, |h, _| h.connection(id).is_some()));
    assert!(!session.is_closed(), "nothing ended silently");

    // "Roll back and close": the session closes, then the connection is deleted.
    vcx.update(|window, cx| window.close_dialog(cx));
    ws.update_in(&mut vcx, |ws, window, cx| {
        ws.delete_connection_ending_transactions(id, false, window, cx);
    });
    wait_until(&mut vcx, |vcx| {
        hub.read_with(vcx, |h, _| h.connection(id).is_none())
    });
    wait_until(&mut vcx, |_| session.is_closed());
}

#[gpui::test]
fn a_confirmation_that_vanishes_ends_the_run(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let (_ws, tab, mut vcx) = connected(cx, ConnectionTypeId::prod(), tmp.path());
    run(&tab, "DELETE FROM t", ExecutionKind::Statement, &mut vcx);
    wait_until(&mut vcx, |vcx| {
        tab.read_with(vcx, |t, _| t.awaiting_confirmation())
    });
    // Closed by other code: neither "Run" nor "Cancel" was called.
    vcx.update(|window, cx| window.close_all_dialogs(cx));
    for _ in 0..50 {
        vcx.executor()
            .advance_clock(std::time::Duration::from_millis(100));
        vcx.run_until_parked();
        if !tab.read_with(&vcx, |t, _| t.is_running()) {
            break;
        }
    }
    assert!(!tab.read_with(&vcx, |t, _| t.is_running()), "the run ended");
    assert!(update_counts(&tab, &vcx).is_empty(), "not run");
    assert!(message(&tab, &vcx).contains("without an answer"));
    let editor = tab.read_with(&vcx, |t, _| t.editor().clone());
    assert!(editor.read_with(&vcx, |e, _| e.running().is_none()));
}

#[test]
fn the_cancel_label_promises_a_server_cancel_only_when_declared() {
    use ninesql_core::driver_api::CancelSupport;
    assert_eq!(
        super::cancel_label_for(Some(CancelSupport::ServerSide)),
        "Cancel"
    );
    assert_eq!(
        super::cancel_label_for(Some(CancelSupport::LocalOnly)),
        "Stop fetching"
    );
    assert_eq!(super::cancel_label_for(None), "Cancel");
}

#[gpui::test]
fn a_result_closed_after_idle_keeps_its_rows_and_hints_at_the_open_transaction(
    cx: &mut TestAppContext,
) {
    use ninesql_core::RunEvent;
    use ninesql_grid::FetchState;
    use std::time::Duration;

    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let live = mock_connection("Live", ConnectionTypeId::dev(), Auth::None);
    let (_ws, _hub, tab, mut vcx) = connected_with(cx, vec![live], tmp.path());
    run(&tab, "BEGIN", ExecutionKind::Statement, &mut vcx);
    wait_until(&mut vcx, |vcx| !tab.read_with(vcx, |t, _| t.is_running()));
    assert!(in_transaction(&tab, &vcx));

    // A paged read pauses after its first page.
    run(
        &tab,
        "SELECT * FROM numbers(1000000)",
        ExecutionKind::Statement,
        &mut vcx,
    );
    wait_until(&mut vcx, |vcx| {
        tab.read_with(vcx, |t, cx| {
            t.results()
                .read(cx)
                .results()
                .tabs()
                .first()
                .is_some_and(|r| match r {
                    ResultTab::Table(m) => m.borrow().state() == FetchState::Idle,
                    ResultTab::Documents { .. } => false,
                })
        })
    });
    let loaded = rows(&tab, &vcx)[0];
    // What the core sends when the paused result waited past the idle limit.
    tab.update_in(&mut vcx, |t, window, cx| {
        let generation = t.generation;
        t.on_run_event(
            generation,
            RunEvent::ResultClosedIdle {
                index: 0,
                rows: loaded as u64,
                after: Duration::from_secs(240),
            },
            window,
            cx,
        );
        t.on_run_event(
            generation,
            RunEvent::StatementClosed {
                index: 0,
                rows: loaded as u64,
            },
            window,
            cx,
        );
    });
    vcx.run_until_parked();
    assert_eq!(
        message(&tab, &vcx),
        "Result closed after 4 min idle: the rows already loaded are kept. Run the statement \
         again to read more. The transaction is still open on this session: commit or roll back \
         when you are done."
    );
    let results = tab.read_with(&vcx, |t, _| t.results().clone());
    results.read_with(&vcx, |r, _| {
        let Some(ResultTab::Table(m)) = r.results().tabs().first() else {
            panic!("a table");
        };
        assert_eq!(
            m.borrow().state(),
            FetchState::ClosedIdle { after_secs: 240 }
        );
        assert_eq!(m.borrow().row_count(), loaded, "rows kept");
        assert!(
            r.banner(0)
                .is_some_and(|b| b.starts_with("Result closed after 4 min idle"))
        );
    });
    // Fetching is off for that result (I8).
    results.update(&mut vcx, |r, cx| r.fetch(0, true, cx));
    results.read_with(&vcx, |r, _| {
        let Some(ResultTab::Table(m)) = r.results().tabs().first() else {
            panic!("a table");
        };
        assert!(!m.borrow().state().is_fetching());
    });
    // Without a transaction, no hint.
    assert!(!super::idle_close_message(Duration::from_secs(240), false).contains("transaction"));

    tab.update_in(&mut vcx, |t, window, cx| t.cancel_run(window, cx));
    wait_until(&mut vcx, |vcx| !tab.read_with(vcx, |t, _| t.is_running()));
}
