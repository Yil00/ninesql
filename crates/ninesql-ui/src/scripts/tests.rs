//! `TestAppContext` tests of script files (F-SQL-11) and session restore (F-WS-06): the
//! actions and their keys, save / open round trip, the dirty marker and the close
//! confirmation, the session file holding no editor text (I1), unsaved tabs not restored, a
//! missing file skipped, and no connection opened by a restore (I3).

use std::path::Path;
use std::sync::Arc;

use gpui::{Entity, TestAppContext, VisualTestContext};
use gpui_kit::component::WindowExt as _;
use ninesql_core::driver_api::mock::MockDriver;
use ninesql_core::secrets::SecretStores;
use ninesql_core::storage::{Paths, ProjectName, XdgDirs};
use ninesql_core::{
    Auth, ConnectionId, ConnectionTypeId, DbeaverRoots, KnownHostsFiles, ManagerConfig,
    SessionManager, Workspace as CoreWorkspace,
};
use ninesql_editor::script::ScriptPath;

use super::session::{self, SessionFile};
use super::store::ScriptStore;
use super::{SessionState, parse_script_path};
use crate::app;
use crate::connections::hub::{ConnectionHub, HubConfig, install, registry_with};
use crate::connections::tests::{mock_connection, wait_until};
use crate::layout::TabId;
use crate::settings::{self, SettingsState};
use crate::workspace::Workspace;

fn paths(base: &Path) -> Paths {
    let base = base.to_path_buf();
    let xdg = XdgDirs::from_lookup(|k| {
        let dir = match k {
            "XDG_CONFIG_HOME" => "config",
            "XDG_DATA_HOME" => "data",
            "XDG_STATE_HOME" => "state",
            "HOME" => "home",
            _ => return None,
        };
        Some(base.join(dir).into_os_string())
    })
    .unwrap_or_else(|e| panic!("{e}"));
    Paths::new(&xdg)
}

fn store(base: &Path) -> ScriptStore {
    ScriptStore::new(
        &paths(base).project(&ProjectName::default_project()),
        DbeaverRoots::none(),
    )
}

fn sp(p: &str) -> ScriptPath {
    ScriptPath::new(p).unwrap_or_else(|e| panic!("{e}"))
}

/// The app with a hub on a workspace under `base`, the mock driver and `connections`.
fn init(
    cx: &mut TestAppContext,
    base: &Path,
    connections: Vec<ninesql_core::Connection>,
) -> Entity<ConnectionHub> {
    let (settings, diagnostics) = settings::parse("");
    let mut manager_config = ManagerConfig::with_known_hosts(KnownHostsFiles {
        own: base.join("known_hosts"),
        user: None,
    });
    manager_config.workers = 1;
    let registry = registry_with(vec![Arc::new(MockDriver::new())]);
    let manager = SessionManager::new(manager_config, registry, SecretStores::new())
        .unwrap_or_else(|e| panic!("{e}"));
    let workspace = CoreWorkspace::open(paths(base), ProjectName::default_project())
        .unwrap_or_else(|e| panic!("{e}"));
    let mut config = HubConfig::empty();
    config.registry = manager.registry().clone();
    config.manager = Some(manager);
    config.workspace = Some(workspace);
    config.connections =
        ninesql_core::Connections::from_vec(connections).unwrap_or_else(|e| panic!("{e}"));
    cx.update(|cx| {
        app::init(
            SettingsState {
                path: None,
                settings,
                diagnostics,
                save_blocked: None,
            },
            cx,
        );
        install(config, cx)
    })
}

fn open(cx: &mut TestAppContext) -> (Entity<Workspace>, VisualTestContext) {
    let Some((window, ws)) = cx.update(app::open_window) else {
        panic!("the test platform opens windows");
    };
    let vcx = VisualTestContext::from_window(window, cx);
    vcx.executor().allow_parking();
    vcx.run_until_parked();
    (ws, vcx)
}

fn active_tab(ws: &Entity<Workspace>, vcx: &VisualTestContext) -> TabId {
    ws.read_with(vcx, |ws, _| ws.layout().active_tab())
        .unwrap_or_else(|| panic!("an active tab"))
}

fn set_text(ws: &Entity<Workspace>, tab: TabId, text: &str, vcx: &mut VisualTestContext) {
    let Some(editor) = ws.read_with(vcx, |ws, _| ws.editor(tab).cloned()) else {
        panic!("editor tab");
    };
    let text = text.to_owned();
    vcx.update(|window, cx| {
        editor.update(cx, |e, cx| {
            e.editor().update(cx, |ed, cx| {
                ed.set_text(&text, window, cx);
            });
        });
    });
    vcx.run_until_parked();
}

fn text_of(ws: &Entity<Workspace>, tab: TabId, vcx: &VisualTestContext) -> String {
    ws.read_with(vcx, |ws, cx| {
        ws.editor(tab)
            .map(|e| e.read(cx).editor().read(cx).text(cx))
            .unwrap_or_default()
    })
}

fn is_dirty(ws: &Entity<Workspace>, tab: TabId, vcx: &VisualTestContext) -> bool {
    ws.read_with(vcx, |ws, cx| {
        ws.editor(tab).is_some_and(|e| e.read(cx).is_dirty(cx))
    })
}

fn label(ws: &Entity<Workspace>, tab: TabId, vcx: &VisualTestContext) -> String {
    ws.read_with(vcx, |ws, cx| {
        ws.tab_label(tab, cx)
            .map(|l| l.to_string())
            .unwrap_or_default()
    })
}

fn has_dialog(vcx: &mut VisualTestContext) -> bool {
    vcx.update(|window, cx| window.has_active_dialog(cx))
}

fn close_dialog(vcx: &mut VisualTestContext) {
    vcx.update(|window, cx| window.close_dialog(cx));
    vcx.run_until_parked();
}

fn save_to(ws: &Entity<Workspace>, tab: TabId, path: &str, vcx: &mut VisualTestContext) {
    let path = sp(path);
    ws.update_in(vcx, |ws, window, cx| {
        ws.save_tab_to(tab, path, true, None, window, cx);
    });
    vcx.run_until_parked();
}

fn walk(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push(p);
            }
        }
    }
    out
}

#[test]
fn script_actions_have_keys_and_palette_entries() {
    for name in [
        "NewScript",
        "OpenScript",
        "SaveScript",
        "SaveScriptAs",
        "ConnectScriptConnection",
    ] {
        let spec = crate::actions::find(name).unwrap_or_else(|| panic!("{name}"));
        assert!(spec.in_palette, "{name}");
        assert!(!spec.default_keys.is_empty(), "{name}");
    }
    let save = crate::actions::find("SaveScript").unwrap_or_else(|| panic!("SaveScript"));
    assert_eq!(save.default_keys, ["ctrl-s"]);
}

#[test]
fn typed_names_become_script_paths() {
    assert_eq!(
        parse_script_path("reports/monthly").ok(),
        Some(sp("reports/monthly.sql"))
    );
    assert_eq!(parse_script_path(" /a.sql ").ok(), Some(sp("a.sql")));
    for bad in ["", "../x", "a/../../x.sql", ".hidden", "x.txt"] {
        assert!(parse_script_path(bad).is_err(), "{bad}");
    }
}

#[gpui::test]
fn every_workspace_action_is_handled_from_the_editor(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    init(cx, tmp.path(), Vec::new());
    let (_ws, mut vcx) = open(cx);
    for spec in crate::actions::registry()
        .iter()
        .filter(|s| s.scope == crate::actions::Scope::Workspace)
    {
        let action = (spec.build)();
        let available = vcx.update(|window, cx| window.is_action_available(action.as_ref(), cx));
        assert!(available, "{} has no handler on the key path", spec.name());
    }
    // The check can fail: a grid action is not handled from the editor.
    let grid = vcx
        .update(|window, cx| window.is_action_available(&ninesql_grid::actions::CopySelection, cx));
    assert!(!grid);
}

#[gpui::test]
fn save_then_open_round_trip_with_dirty_marker(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    init(cx, tmp.path(), Vec::new());
    let (ws, mut vcx) = open(cx);
    let tab = active_tab(&ws, &vcx);
    assert!(
        !is_dirty(&ws, tab, &vcx),
        "an empty untitled editor is clean"
    );
    set_text(&ws, tab, "SELECT 1;\n", &mut vcx);
    assert!(is_dirty(&ws, tab, &vcx));
    assert_eq!(label(&ws, tab, &vcx), "● Script-1");
    // Ctrl+S on an untitled editor asks for a name.
    vcx.simulate_keystrokes("ctrl-s");
    assert!(has_dialog(&mut vcx), "Save as asks for a name");
    close_dialog(&mut vcx);
    save_to(&ws, tab, "reports/q.sql", &mut vcx);
    let file = store(tmp.path()).dir().join("reports/q.sql");
    assert_eq!(
        std::fs::read_to_string(&file).ok().as_deref(),
        Some("SELECT 1;\n")
    );
    let mode = std::os::unix::fs::PermissionsExt::mode(
        &std::fs::metadata(&file)
            .unwrap_or_else(|e| panic!("{e}"))
            .permissions(),
    );
    assert_eq!(mode & 0o777, 0o600);
    assert!(!is_dirty(&ws, tab, &vcx));
    assert_eq!(label(&ws, tab, &vcx), "q.sql");
    // Edit, then Ctrl+S writes the file at once.
    set_text(&ws, tab, "SELECT 2;\n", &mut vcx);
    assert_eq!(label(&ws, tab, &vcx), "● q.sql");
    vcx.simulate_keystrokes("ctrl-s");
    vcx.run_until_parked();
    assert!(!has_dialog(&mut vcx));
    assert_eq!(
        std::fs::read_to_string(&file).ok().as_deref(),
        Some("SELECT 2;\n")
    );
    assert!(!is_dirty(&ws, tab, &vcx));
    // Back to the saved text by hand: clean again.
    set_text(&ws, tab, "SELECT 3;\n", &mut vcx);
    assert!(is_dirty(&ws, tab, &vcx));
    set_text(&ws, tab, "SELECT 2;\n", &mut vcx);
    assert!(!is_dirty(&ws, tab, &vcx), "the text matches the file again");
    // Close (no question: nothing unsaved) and open it again.
    vcx.simulate_keystrokes("ctrl-w");
    vcx.run_until_parked();
    assert!(!has_dialog(&mut vcx));
    assert!(ws.read_with(&vcx, |ws, _| ws.editor(tab).is_none()));
    let task = ws.update_in(&mut vcx, |ws, window, cx| {
        ws.open_script_path(sp("reports/q.sql"), window, cx)
    });
    vcx.run_until_parked();
    let Some(opened) = vcx.foreground_executor().block_test(task) else {
        panic!("opened");
    };
    assert_eq!(text_of(&ws, opened, &vcx), "SELECT 2;\n");
    assert!(!is_dirty(&ws, opened, &vcx));
    assert_eq!(
        ws.read_with(&vcx, |ws, cx| ws.active_title(cx)).as_deref(),
        Some("q.sql")
    );
    // Opening it again activates the same tab.
    let again = ws.update_in(&mut vcx, |ws, window, cx| {
        ws.open_script_path(sp("reports/q.sql"), window, cx)
    });
    assert_eq!(vcx.foreground_executor().block_test(again), Some(opened));
}

#[gpui::test]
fn loading_normalises_line_ends_and_shows_the_file_as_changed(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    init(cx, tmp.path(), Vec::new());
    let (ws, mut vcx) = open(cx);
    let s = store(tmp.path());
    s.write(
        &sp("crlf.sql"),
        "SELECT 1;\r\nSELECT\u{7}2;\r\n",
        None,
        true,
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let task = ws.update_in(&mut vcx, |ws, window, cx| {
        ws.open_script_path(sp("crlf.sql"), window, cx)
    });
    vcx.run_until_parked();
    let Some(tab) = vcx.foreground_executor().block_test(task) else {
        panic!("opened");
    };
    assert_eq!(text_of(&ws, tab, &vcx), "SELECT 1;\nSELECT2;\n");
    assert!(
        is_dirty(&ws, tab, &vcx),
        "the cleaned text differs from the file"
    );
}

#[gpui::test]
fn closing_an_unsaved_tab_asks_first(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    init(cx, tmp.path(), Vec::new());
    let (ws, mut vcx) = open(cx);
    let tab = active_tab(&ws, &vcx);
    set_text(&ws, tab, "SELECT 'draft';", &mut vcx);
    vcx.simulate_keystrokes("ctrl-w");
    vcx.run_until_parked();
    assert!(has_dialog(&mut vcx), "asks before losing unsaved text");
    assert!(ws.read_with(&vcx, |ws, _| ws.editor(tab).is_some()));
    // Cancel keeps the tab.
    close_dialog(&mut vcx);
    assert!(ws.read_with(&vcx, |ws, _| ws.editor(tab).is_some()));
    assert_eq!(ws.read_with(&vcx, |ws, cx| ws.dirty_tab_count(cx)), 1);
    // "Discard changes" closes it.
    ws.update_in(&mut vcx, |ws, window, cx| {
        ws.close_tab_checked(tab, true, window, cx)
    });
    assert!(ws.read_with(&vcx, |ws, _| ws.editor(tab).is_none()));
    // Closing the window with unsaved text asks too.
    vcx.simulate_keystrokes("ctrl-t");
    let tab = active_tab(&ws, &vcx);
    set_text(&ws, tab, "SELECT 'draft 2';", &mut vcx);
    let closes = ws.update_in(&mut vcx, |ws, window, cx| {
        ws.should_close_window(window, cx)
    });
    assert!(!closes);
    assert!(has_dialog(&mut vcx));
}

#[gpui::test]
fn new_and_open_script_actions(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    init(cx, tmp.path(), Vec::new());
    let (ws, mut vcx) = open(cx);
    vcx.simulate_keystrokes("ctrl-n");
    vcx.run_until_parked();
    let tab = active_tab(&ws, &vcx);
    assert_eq!(
        ws.read_with(&vcx, |ws, cx| ws.active_title(cx)).as_deref(),
        Some("Script-2.sql")
    );
    assert!(!is_dirty(&ws, tab, &vcx));
    assert!(store(tmp.path()).dir().join("Script-2.sql").is_file());
    vcx.simulate_keystrokes("ctrl-o");
    vcx.run_until_parked();
    assert!(has_dialog(&mut vcx), "the script picker");
    close_dialog(&mut vcx);
    vcx.simulate_keystrokes("ctrl-shift-s");
    vcx.run_until_parked();
    assert!(has_dialog(&mut vcx), "the save-as prompt");
}

#[gpui::test]
fn symlinked_and_dbeaver_scripts_are_refused_in_the_ui(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let hub = init(cx, tmp.path(), Vec::new());
    let (ws, mut vcx) = open(cx);
    let outside = tmp.path().join("outside.sql");
    std::fs::write(&outside, "SELECT 'outside';").unwrap_or_else(|e| panic!("{e}"));
    let dir = store(tmp.path()).dir().to_path_buf();
    std::os::unix::fs::symlink(&outside, dir.join("link.sql")).unwrap_or_else(|e| panic!("{e}"));
    let task = ws.update_in(&mut vcx, |ws, window, cx| {
        ws.open_script_path(sp("link.sql"), window, cx)
    });
    vcx.run_until_parked();
    assert!(vcx.foreground_executor().block_test(task).is_none());
    let tab = active_tab(&ws, &vcx);
    set_text(&ws, tab, "DROP TABLE t;", &mut vcx);
    let path = sp("link.sql");
    ws.update_in(&mut vcx, |ws, window, cx| {
        ws.save_tab_to(tab, path, false, None, window, cx);
    });
    vcx.run_until_parked();
    assert_eq!(
        std::fs::read_to_string(&outside).ok().as_deref(),
        Some("SELECT 'outside';"),
        "never written through a link"
    );
    assert!(is_dirty(&ws, tab, &vcx), "not saved");
    // The session manager's DBeaver roots apply (I4).
    hub.read_with(&vcx, |h, _| {
        if let Some(m) = h.manager() {
            m.dbeaver_roots().add(tmp.path().join("data"));
        }
    });
    save_to(&ws, tab, "inside.sql", &mut vcx);
    assert!(!dir.join("inside.sql").exists());
    assert!(is_dirty(&ws, tab, &vcx));
}

#[gpui::test]
fn the_session_file_holds_no_editor_text_and_skips_unsaved_tabs(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let c = mock_connection("Orders", ConnectionTypeId::dev(), Auth::None);
    let connection = c.id;
    init(cx, tmp.path(), vec![c]);
    let (ws, mut vcx) = open(cx);
    let p = paths(tmp.path());
    // An untitled tab with a typed password (never saved), a saved script with unsaved edits,
    // and a saved script in a split.
    let scratch = active_tab(&ws, &vcx);
    set_text(
        &ws,
        scratch,
        "ALTER USER app PASSWORD 'hunter2-untitled';",
        &mut vcx,
    );
    vcx.simulate_keystrokes("ctrl-t");
    let saved = active_tab(&ws, &vcx);
    set_text(&ws, saved, "SELECT 1;\nSELECT 2;\n", &mut vcx);
    save_to(&ws, saved, "a.sql", &mut vcx);
    set_text(
        &ws,
        saved,
        "SELECT 1;\n-- s3cr3t-unsaved-edit\nSELECT 2;\n",
        &mut vcx,
    );
    let editor = ws.read_with(&vcx, |ws, _| ws.editor(saved).cloned());
    if let Some(editor) = editor {
        editor.update(&mut vcx, |e, cx| {
            e.set_linked(Some(connection), cx);
            e.editor()
                .update(cx, |ed, cx| ed.set_cursor_line_column(1, 3, cx));
        });
    }
    vcx.simulate_keystrokes("ctrl-\\");
    let split = active_tab(&ws, &vcx);
    set_text(&ws, split, "SELECT 'b';", &mut vcx);
    save_to(&ws, split, "dir/b.sql", &mut vcx);

    // The quit path: written once the restore ran.
    assert!(
        vcx.update(|_, cx| SessionState::current(cx)).is_none(),
        "not before the restore"
    );
    vcx.update(|_, cx| SessionState::mark_restored(false, cx));
    let Some(file) = vcx.update(|_, cx| SessionState::current(cx)) else {
        panic!("a session");
    };
    session::write(&p.session_file(), Some(&store(tmp.path())), file)
        .unwrap_or_else(|e| panic!("{e}"));
    let json = std::fs::read_to_string(p.session_file()).unwrap_or_default();
    for secret in ["hunter2", "s3cr3t", "SELECT", "ALTER"] {
        assert!(
            !json.contains(secret),
            "{secret} in the session file: {json}"
        );
    }
    assert!(
        json.contains("a.sql") && json.contains("dir/b.sql"),
        "{json}"
    );
    assert!(json.contains(&connection.to_string()));
    // I1: no unsaved text in any file NineSQL wrote.
    for f in walk(tmp.path()) {
        let bytes = std::fs::read(&f).unwrap_or_default();
        let text = String::from_utf8_lossy(&bytes);
        assert!(!text.contains("hunter2"), "{}", f.display());
        assert!(!text.contains("s3cr3t"), "{}", f.display());
    }
    let plan = session::read_plan(&p.session_file(), &store(tmp.path()), "General");
    assert!(plan.notices.is_empty(), "{:?}", plan.notices);
    let names: Vec<String> = plan.windows[0]
        .tabs
        .iter()
        .map(|t| t.script.path.as_path().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        names,
        ["a.sql", "dir/b.sql"],
        "the untitled tab is not restored"
    );
    assert_eq!(plan.windows[0].tabs[0].cursor, (1, 3));
    assert_eq!(
        plan.windows[0].tabs[0].script.text,
        "SELECT 1;\nSELECT 2;\n"
    );
}

#[gpui::test]
fn restore_reopens_saved_scripts_without_connecting(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let c = mock_connection("Orders", ConnectionTypeId::dev(), Auth::None);
    let connection = c.id;
    let hub = init(cx, tmp.path(), vec![c]);
    let s = store(tmp.path());
    s.write(
        &sp("a.sql"),
        "SELECT 1;\nSELECT 22;\n",
        Some(connection),
        true,
    )
    .unwrap_or_else(|e| panic!("{e}"));
    s.write(&sp("b.sql"), "SELECT 'b';", None, true)
        .unwrap_or_else(|e| panic!("{e}"));
    let tab = |script: &str, connection: Option<ConnectionId>| ninesql_editor::script::SessionTab {
        script: sp(script),
        connection,
        cursor_line: 1,
        cursor_column: 8,
        scroll_line: 0,
        stamp: None,
    };
    let file = SessionFile {
        project: "General".to_owned(),
        windows: vec![session::WindowSession {
            editor: ninesql_editor::script::EditorSession::new(
                vec![
                    tab("a.sql", Some(connection)),
                    tab("gone.sql", None),
                    tab("b.sql", None),
                ],
                Some(0),
            ),
            layout: crate::layout::LayoutSnapshot::Split {
                axis: crate::layout::SnapshotAxis::Horizontal,
                children: vec![
                    crate::layout::LayoutSnapshot::Pane {
                        tabs: vec![0, 1],
                        active: 0,
                        focused: true,
                    },
                    crate::layout::LayoutSnapshot::Pane {
                        tabs: vec![2],
                        active: 0,
                        focused: false,
                    },
                ],
            },
            navigator_visible: false,
        }],
    };
    let p = paths(tmp.path());
    session::write(&p.session_file(), Some(&s), file).unwrap_or_else(|e| panic!("{e}"));
    std::fs::remove_file(s.dir().join("gone.sql")).ok();
    let (ws, mut vcx) = open(cx);
    // The application's start-up path.
    vcx.update(|_, cx| super::install(cx));
    wait_until(&mut vcx, |vcx| {
        ws.read_with(vcx, |ws, _| ws.layout().all_tabs().len() == 2)
    });
    ws.read_with(&vcx, |ws, cx| {
        assert_eq!(ws.layout().pane_count(), 2, "the split is back");
        assert!(!ws.navigator_visible());
        let titles: Vec<String> = ws.tab_titles(cx).iter().map(ToString::to_string).collect();
        assert_eq!(
            titles,
            ["a.sql", "b.sql"],
            "the empty start-up tab was replaced"
        );
        assert_eq!(ws.active_title(cx).as_deref(), Some("a.sql"));
        assert_eq!(ws.dirty_tab_count(cx), 0);
    });
    let a = active_tab(&ws, &vcx);
    assert_eq!(text_of(&ws, a, &vcx), "SELECT 1;\nSELECT 22;\n");
    ws.read_with(&vcx, |ws, cx| {
        let Some(editor) = ws.editor(a) else {
            panic!("a.sql")
        };
        let editor = editor.read(cx);
        assert_eq!(editor.editor().read(cx).cursor_line_column(cx), (1, 8));
        // Not connected: shown with its connection, no session opened (I3).
        assert!(editor.session().is_none());
        assert_eq!(editor.linked_not_connected(), Some(connection));
    });
    assert!(hub.read_with(&vcx, |h, _| h.open_sessions().is_empty()));
    assert!(!ws.read_with(&vcx, |ws, cx| ws.flow().read(cx).is_busy()));
    // Connecting is the user's choice, through the normal flow; the tab takes the session.
    vcx.simulate_keystrokes("ctrl-k ctrl-shift-o");
    wait_until(&mut vcx, |vcx| {
        ws.read_with(vcx, |ws, cx| {
            ws.editor(a).is_some_and(|e| e.read(cx).session().is_some())
        })
    });
    assert_eq!(hub.read_with(&vcx, |h, _| h.open_sessions().len()), 1);
    // The session file is written at quit from now on.
    assert!(vcx.update(|_, cx| SessionState::current(cx)).is_some());
}
