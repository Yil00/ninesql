//! View-model tests on GPUI's `TestAppContext` (AGENTS.md: no pixel tests): actions and their
//! default bindings, key binding overrides, tabs and splits, the navigator dock, the command
//! palette, multiple windows, theme switching, fonts and contrast.

use gpui::{
    Action as _, AnyWindowHandle, Entity, Focusable as _, TestAppContext, VisualTestContext,
};
use gpui_kit::component::{ActiveTheme as _, Theme, ThemeMode, WindowExt as _};

use crate::actions::{self, ACTIONS, CloseWindow, NewEditorTab, NewWindow};
use crate::app;
use crate::appearance::{AppearanceState, contrast_ratio};
use crate::layout::Node;
use crate::settings::{self, SettingsState};
use crate::workspace::Workspace;

fn init(cx: &mut TestAppContext, settings_toml: &str) {
    let (settings, diagnostics) = settings::parse(settings_toml);
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
    });
}

fn open(cx: &mut TestAppContext) -> (AnyWindowHandle, Entity<Workspace>, VisualTestContext) {
    let opened = cx.update(app::open_window);
    let Some((window, workspace)) = opened else {
        panic!("the test platform opens windows");
    };
    let vcx = VisualTestContext::from_window(window, cx);
    vcx.run_until_parked();
    (window, workspace, vcx)
}

fn tab_count(ws: &Entity<Workspace>, cx: &mut VisualTestContext) -> usize {
    ws.read_with(cx, |ws, _| ws.layout().all_tabs().len())
}

fn pane_count(ws: &Entity<Workspace>, cx: &mut VisualTestContext) -> usize {
    ws.read_with(cx, |ws, _| ws.layout().pane_count())
}

#[gpui::test]
fn every_registered_action_is_known_to_gpui(cx: &mut TestAppContext) {
    init(cx, "");
    let names: Vec<&'static str> = cx.update(|cx| {
        cx.all_action_names()
            .iter()
            .copied()
            .filter(|n| n.starts_with("ninesql::"))
            .collect()
    });
    for spec in ACTIONS {
        assert!(
            names.contains(&spec.name()),
            "{} not registered",
            spec.name()
        );
    }
    assert_eq!(
        names.len(),
        ACTIONS.len(),
        "an action is missing from the registry"
    );
}

#[gpui::test]
fn default_bindings_are_installed(cx: &mut TestAppContext) {
    init(cx, "");
    cx.update(|cx| {
        let keymap = cx.key_bindings();
        let keymap = keymap.borrow();
        for spec in ACTIONS {
            let action = (spec.build)();
            let bound = keymap.bindings_for_action(action.as_ref()).count();
            assert_eq!(bound, spec.default_keys.len(), "{}", spec.name());
        }
    });
    let problems = cx.read_global::<SettingsState, _>(|s, _| s.diagnostics.len());
    assert_eq!(problems, 0);
}

#[gpui::test]
fn window_opens_with_one_editor_and_the_navigator(cx: &mut TestAppContext) {
    init(cx, "");
    let (_, ws, mut vcx) = open(cx);
    assert_eq!(tab_count(&ws, &mut vcx), 1);
    ws.read_with(&vcx, |ws, cx| {
        assert!(ws.navigator_visible());
        assert!(ws.navigator().read(cx).is_empty(cx));
        assert_eq!(ws.active_title(cx).as_deref(), Some("Script-1"));
    });
    assert_eq!(vcx.window_title().as_deref(), Some("Script-1 - NineSQL"));
}

#[gpui::test]
fn tabs_through_key_bindings(cx: &mut TestAppContext) {
    init(cx, "");
    let (_, ws, mut vcx) = open(cx);
    vcx.simulate_keystrokes("ctrl-t ctrl-t");
    assert_eq!(tab_count(&ws, &mut vcx), 3);
    ws.read_with(&vcx, |ws, cx| {
        assert_eq!(ws.active_title(cx).as_deref(), Some("Script-3"));
    });
    vcx.simulate_keystrokes("ctrl-tab");
    ws.read_with(&vcx, |ws, cx| {
        assert_eq!(ws.active_title(cx).as_deref(), Some("Script-1"));
    });
    vcx.simulate_keystrokes("ctrl-shift-tab ctrl-pageup");
    ws.read_with(&vcx, |ws, cx| {
        assert_eq!(ws.active_title(cx).as_deref(), Some("Script-2"));
    });
    vcx.simulate_keystrokes("ctrl-w");
    assert_eq!(tab_count(&ws, &mut vcx), 2);
    ws.read_with(&vcx, |ws, cx| {
        assert_eq!(
            ws.tab_titles(cx)
                .iter()
                .map(|t| t.as_ref())
                .collect::<Vec<_>>(),
            ["Script-1", "Script-3"]
        );
    });
    vcx.simulate_keystrokes("ctrl-w ctrl-w");
    assert_eq!(tab_count(&ws, &mut vcx), 0);
    assert_eq!(pane_count(&ws, &mut vcx), 1);
    assert_eq!(vcx.window_title().as_deref(), Some("NineSQL"));
    // The empty workspace keeps focus, so its bindings still work.
    vcx.simulate_keystrokes("ctrl-t");
    assert_eq!(tab_count(&ws, &mut vcx), 1);
}

#[gpui::test]
fn splits_through_key_bindings(cx: &mut TestAppContext) {
    init(cx, "");
    let (_, ws, mut vcx) = open(cx);
    vcx.simulate_keystrokes("ctrl-\\");
    assert_eq!(pane_count(&ws, &mut vcx), 2);
    vcx.simulate_keystrokes("ctrl-k ctrl-\\");
    assert_eq!(pane_count(&ws, &mut vcx), 3);
    ws.read_with(&vcx, |ws, _| {
        let Node::Split { children, .. } = ws.layout().root() else {
            panic!("expected a split");
        };
        assert_eq!(children.len(), 2);
        assert!(
            matches!(children[1], Node::Split { .. }),
            "split down nests"
        );
    });
    let active = ws.read_with(&vcx, |ws, _| ws.layout().active_pane_id());
    vcx.simulate_keystrokes("ctrl-k ctrl-right");
    let first = ws.read_with(&vcx, |ws, _| ws.layout().pane_order()[0]);
    assert_eq!(
        ws.read_with(&vcx, |ws, _| ws.layout().active_pane_id()),
        first,
        "focus wraps to the first pane"
    );
    vcx.simulate_keystrokes("ctrl-k ctrl-left");
    assert_eq!(
        ws.read_with(&vcx, |ws, _| ws.layout().active_pane_id()),
        active
    );
    // Closing the only tab of a split pane removes the pane.
    vcx.simulate_keystrokes("ctrl-w");
    assert_eq!(pane_count(&ws, &mut vcx), 2);
    assert_eq!(tab_count(&ws, &mut vcx), 2);
}

#[gpui::test]
fn navigator_dock_toggles(cx: &mut TestAppContext) {
    init(cx, "");
    let (_, ws, mut vcx) = open(cx);
    vcx.simulate_keystrokes("ctrl-b");
    assert!(!ws.read_with(&vcx, |ws, _| ws.navigator_visible()));
    vcx.simulate_keystrokes("ctrl-b");
    assert!(ws.read_with(&vcx, |ws, _| ws.navigator_visible()));
    vcx.simulate_keystrokes("ctrl-b ctrl-k ctrl-n");
    let focused = ws.update_in(&mut vcx, |ws, window, cx| {
        ws.navigator().focus_handle(cx).contains_focused(window, cx)
    });
    assert!(focused, "focus navigator shows and focuses it");
    assert!(ws.read_with(&vcx, |ws, _| ws.navigator_visible()));
    // Workspace bindings still apply from the navigator.
    vcx.simulate_keystrokes("ctrl-t");
    assert_eq!(tab_count(&ws, &mut vcx), 2);
}

#[gpui::test]
fn overrides_replace_default_bindings(cx: &mut TestAppContext) {
    init(
        cx,
        r#"
[keybindings]
NewEditorTab = "alt-n"
ToggleNavigator = []
"ninesql::SplitRight" = ["ctrl-alt-s", "f6"]
"#,
    );
    let (_, ws, mut vcx) = open(cx);
    vcx.simulate_keystrokes("ctrl-t");
    assert_eq!(tab_count(&ws, &mut vcx), 1, "the default binding is gone");
    vcx.simulate_keystrokes("alt-n");
    assert_eq!(tab_count(&ws, &mut vcx), 2);
    vcx.simulate_keystrokes("ctrl-b");
    assert!(
        ws.read_with(&vcx, |ws, _| ws.navigator_visible()),
        "unbound"
    );
    vcx.simulate_keystrokes("f6");
    assert_eq!(pane_count(&ws, &mut vcx), 2);
    vcx.simulate_keystrokes("ctrl-\\");
    assert_eq!(
        pane_count(&ws, &mut vcx),
        2,
        "the default split binding is gone"
    );
}

#[gpui::test]
fn invalid_overrides_are_reported_and_defaults_kept(cx: &mut TestAppContext) {
    init(
        cx,
        r#"
[keybindings]
NewEditorTab = "ctrl-nope"
Frobnicate = "ctrl-f"
"#,
    );
    let problems = cx.read_global::<SettingsState, _>(|s, _| s.diagnostics.len());
    assert_eq!(problems, 2);
    let (_, ws, mut vcx) = open(cx);
    vcx.simulate_keystrokes("ctrl-t");
    assert_eq!(
        tab_count(&ws, &mut vcx),
        2,
        "defaults kept for the invalid override"
    );
    // The problems are listed on demand.
    vcx.simulate_keystrokes("ctrl-k ctrl-e");
    assert!(vcx.update(|window, cx| window.has_active_dialog(cx)));
}

#[gpui::test]
fn command_palette_runs_actions(cx: &mut TestAppContext) {
    init(cx, "");
    let (_, ws, mut vcx) = open(cx);
    vcx.simulate_keystrokes("ctrl-shift-p");
    assert!(vcx.update(|window, cx| window.has_active_dialog(cx)));
    vcx.simulate_input("split editor right");
    vcx.simulate_keystrokes("enter");
    assert!(!vcx.update(|window, cx| window.has_active_dialog(cx)));
    assert_eq!(pane_count(&ws, &mut vcx), 2);
    // Escape leaves the palette without running anything.
    vcx.simulate_keystrokes("f1");
    assert!(vcx.update(|window, cx| window.has_active_dialog(cx)));
    vcx.simulate_keystrokes("escape");
    assert!(!vcx.update(|window, cx| window.has_active_dialog(cx)));
    assert_eq!(pane_count(&ws, &mut vcx), 2);
}

#[gpui::test]
fn perform_covers_every_action(cx: &mut TestAppContext) {
    init(cx, "");
    let (_, ws, mut vcx) = open(cx);
    for spec in ACTIONS {
        let name = spec.name();
        if matches!(
            name,
            "ninesql::Quit" | "ninesql::CloseWindow" | "ninesql::NewWindow"
        ) {
            continue;
        }
        let handled = ws.update_in(&mut vcx, |ws, window, cx| {
            ws.perform((spec.build)().as_ref(), window, cx)
        });
        assert!(handled, "{name} is not handled");
        vcx.update(|window, cx| window.close_all_dialogs(cx));
        vcx.run_until_parked();
    }
    assert!(!ws.update_in(&mut vcx, |ws, window, cx| ws.perform(
        &gpui::NoAction,
        window,
        cx
    )));
}

#[gpui::test]
fn multiple_windows(cx: &mut TestAppContext) {
    init(cx, "");
    let (window, ws, mut vcx) = open(cx);
    cx.dispatch_action(window, NewWindow);
    cx.run_until_parked();
    assert_eq!(cx.windows().len(), 2);
    // Each window has its own workspace.
    vcx.simulate_keystrokes("ctrl-t");
    assert_eq!(tab_count(&ws, &mut vcx), 2);
    let others: Vec<_> = cx.windows().into_iter().filter(|w| *w != window).collect();
    assert_eq!(others.len(), 1);
    cx.dispatch_action(others[0], CloseWindow);
    cx.run_until_parked();
    assert_eq!(cx.windows().len(), 1);
    assert_eq!(cx.windows()[0], window);
}

#[gpui::test]
fn theme_switching_keeps_font_settings(cx: &mut TestAppContext) {
    init(
        cx,
        r#"
[appearance]
theme = "dark"
ui_font_size = 15
mono_font_size = 14
mono_font_family = "DejaVu Sans Mono"
"#,
    );
    let (_, _ws, mut vcx) = open(cx);
    let read = |vcx: &mut VisualTestContext| {
        vcx.update(|_, cx| {
            let t = cx.theme();
            (t.mode, f32::from(t.font_size), f32::from(t.mono_font_size))
        })
    };
    // The test platform has no fonts, so gpui-component falls back from the configured family
    // at render time; the configured family is checked in the appearance state.
    let family = vcx.update(|_, cx| {
        cx.global::<AppearanceState>()
            .settings
            .mono_font_family
            .clone()
    });
    assert_eq!(family.as_deref(), Some("DejaVu Sans Mono"));
    assert_eq!(read(&mut vcx), (ThemeMode::Dark, 15.0, 14.0));
    vcx.simulate_keystrokes("ctrl-k ctrl-t");
    assert_eq!(read(&mut vcx), (ThemeMode::Light, 15.0, 14.0));
    vcx.simulate_keystrokes("ctrl-= ctrl-=");
    assert_eq!(read(&mut vcx).1, 17.0);
    assert_eq!(read(&mut vcx).2, 16.0);
    vcx.simulate_keystrokes("ctrl-k ctrl-t");
    assert_eq!(read(&mut vcx).0, ThemeMode::Dark);
    assert_eq!(read(&mut vcx).2, 16.0, "zoom survives a theme switch");
    vcx.simulate_keystrokes("ctrl--");
    assert_eq!(read(&mut vcx).2, 15.0);
    vcx.simulate_keystrokes("ctrl-0");
    assert_eq!(read(&mut vcx).1, 15.0);
    assert_eq!(read(&mut vcx).2, 14.0);
}

#[gpui::test]
fn text_contrast_meets_wcag_aa_in_both_themes(cx: &mut TestAppContext) {
    init(cx, "");
    for mode in [ThemeMode::Light, ThemeMode::Dark] {
        cx.update(|cx| crate::appearance::set_mode(mode, cx));
        cx.update(|cx| {
            let t = cx.theme();
            let pairs = [
                ("foreground / background", t.foreground, t.background),
                ("muted / background", t.muted_foreground, t.background),
                (
                    "muted / status bar",
                    t.muted_foreground,
                    *t.tokens.status_bar,
                ),
                ("sidebar text / sidebar", t.sidebar_foreground, t.sidebar),
                ("muted / sidebar", t.muted_foreground, t.sidebar),
            ];
            for (what, fg, bg) in pairs {
                let ratio = contrast_ratio(fg, bg);
                assert!(ratio >= 4.5, "{mode:?} {what}: {ratio:.2} < 4.5");
            }
        });
    }
    // Theme state is global: switching does not need a window.
    assert!(cx.update(|cx| Theme::global(cx).mode.is_dark()));
}

#[gpui::test]
fn every_action_has_a_name_gpui_can_build(cx: &mut TestAppContext) {
    init(cx, "");
    cx.update(|cx| {
        for spec in ACTIONS {
            let built = cx.build_action(spec.name(), None);
            assert!(built.is_ok(), "{}", spec.name());
        }
    });
    assert_eq!(
        actions::find("NewEditorTab").map(|s| s.name()),
        Some(NewEditorTab.name())
    );
}

// --- Settings saved from the UI (P1-A follow-up) -------------------------------------------

mod persistence {
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use gpui::TestAppContext;
    use ninesql_core::storage::{Paths, XdgDirs};

    use super::{open, tab_count};
    use crate::app;
    use crate::appearance;
    use crate::keymap;
    use crate::persist::{self, DEBOUNCE};
    use crate::settings::{self, SettingsState, ThemePreference};

    /// A temporary `$HOME` and the NineSQL locations under it.
    fn temp_paths() -> (tempfile::TempDir, Paths) {
        let Ok(dir) = tempfile::tempdir() else {
            panic!("no temp dir");
        };
        let home = dir.path().to_path_buf();
        let Ok(xdg) = XdgDirs::from_lookup(|k| (k == "HOME").then(|| home.clone().into())) else {
            panic!("no XDG dirs");
        };
        (dir, Paths::new(&xdg))
    }

    fn write_settings(paths: &Paths, text: &str) -> PathBuf {
        let file = paths.settings_file();
        assert!(std::fs::create_dir_all(paths.config_dir()).is_ok());
        assert!(std::fs::write(&file, text).is_ok());
        file
    }

    fn start(cx: &mut TestAppContext, paths: &Paths) {
        let state = settings::load_from_paths(paths);
        cx.update(|cx| app::init(state, cx));
    }

    /// Lets the debounce timer fire and the background write run.
    fn settle(cx: &mut TestAppContext) {
        cx.executor()
            .advance_clock(DEBOUNCE + Duration::from_millis(100));
        cx.run_until_parked();
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap_or_default()
    }

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).map_or(0, |m| m.permissions().mode() & 0o777)
    }

    const HAND_WRITTEN: &str = "# hand-written\nschema_version = 1\nmystery = \"kept\"\n\n\
        [appearance]\ntheme = \"dark\"  # night owl\nui_font_size = 15\n";

    #[gpui::test]
    fn theme_and_zoom_are_saved_after_the_debounce(cx: &mut TestAppContext) {
        let (_dir, paths) = temp_paths();
        let file = write_settings(&paths, HAND_WRITTEN);
        start(cx, &paths);
        let (_, _ws, mut vcx) = open(cx);
        vcx.simulate_keystrokes("ctrl-k ctrl-t");
        vcx.simulate_keystrokes("ctrl-= ctrl-=");
        cx.run_until_parked();
        // Nothing is written before the debounce delay (and never on the UI thread).
        assert_eq!(read(&file), HAND_WRITTEN);
        assert!(cx.update(|cx| persist::has_pending(cx)));
        settle(cx);
        assert!(!cx.update(|cx| persist::has_pending(cx)));
        let text = read(&file);
        for kept in [
            "# hand-written",
            "schema_version = 1",
            "mystery = \"kept\"",
            "theme = \"light\"  # night owl",
            "ui_font_size = 15",
            "zoom = 2",
        ] {
            assert!(text.contains(kept), "missing {kept:?} in:\n{text}");
        }
        assert_eq!(mode(&file), 0o600);
        // The settings in memory follow, and the file reads back the same.
        let in_memory = cx.read_global::<SettingsState, _>(|s, _| s.settings.clone());
        assert_eq!(in_memory, settings::load(&file).settings);
        assert_eq!(in_memory.appearance.theme, ThemePreference::Light);
    }

    #[gpui::test]
    fn missing_file_and_directory_are_created_private(cx: &mut TestAppContext) {
        let (_dir, paths) = temp_paths();
        start(cx, &paths);
        cx.update(|cx| {
            appearance::set_theme_preference(ThemePreference::System, cx);
            appearance::set_mono_font(Some("  Fira Code ".into()), Some(13.5), cx);
        });
        settle(cx);
        let file = paths.settings_file();
        let text = read(&file);
        assert!(text.starts_with("schema_version = 1\n"), "{text}");
        let loaded = settings::load(&file);
        assert!(loaded.diagnostics.is_empty(), "{:?}", loaded.diagnostics);
        let a = loaded.settings.appearance;
        assert_eq!(a.theme, ThemePreference::System);
        assert_eq!(a.mono_font_family.as_deref(), Some("Fira Code"));
        assert_eq!(a.mono_font_size, Some(13.5));
        assert_eq!(mode(&file), 0o600);
        assert_eq!(mode(paths.config_dir()), 0o700);
    }

    #[gpui::test]
    fn key_binding_overrides_apply_at_once_and_are_saved(cx: &mut TestAppContext) {
        let (_dir, paths) = temp_paths();
        let file = write_settings(&paths, "[keybindings]\nSplitDown = \"alt-shift-2\"\n");
        start(cx, &paths);
        let widget_bindings = |cx: &mut TestAppContext| {
            cx.update(|cx| {
                cx.key_bindings()
                    .borrow()
                    .bindings()
                    .filter(|b| !b.action().name().starts_with("ninesql::"))
                    .count()
            })
        };
        let before = widget_bindings(cx);
        let (_, ws, mut vcx) = open(cx);
        let set =
            cx.update(|cx| keymap::set_override("NewEditorTab", Some(vec!["alt-n".into()]), cx));
        assert!(set.is_ok());
        vcx.run_until_parked();
        vcx.simulate_keystrokes("ctrl-t");
        assert_eq!(tab_count(&ws, &mut vcx), 1, "the default is replaced");
        vcx.simulate_keystrokes("alt-n");
        assert_eq!(tab_count(&ws, &mut vcx), 2);
        assert_eq!(
            widget_bindings(cx),
            before,
            "gpui-component bindings are kept"
        );
        settle(cx);
        let text = read(&file);
        assert!(
            text.contains("\"ninesql::NewEditorTab\" = \"alt-n\""),
            "{text}"
        );
        assert!(text.contains("SplitDown = \"alt-shift-2\""), "{text}");

        // Invalid bindings change nothing.
        let bad =
            cx.update(|cx| keymap::set_override("NewEditorTab", Some(vec!["ctrl-foo".into()]), cx));
        assert!(bad.is_err());

        // Back to the defaults: the entry is removed.
        assert!(
            cx.update(|cx| keymap::set_override("ninesql::NewEditorTab", None, cx))
                .is_ok()
        );
        vcx.run_until_parked();
        vcx.simulate_keystrokes("ctrl-t");
        assert_eq!(tab_count(&ws, &mut vcx), 3);
        settle(cx);
        let text = read(&file);
        assert!(!text.contains("NewEditorTab"), "{text}");
        assert!(text.contains("SplitDown"), "{text}");
    }

    #[gpui::test]
    fn files_that_were_not_read_are_never_overwritten(cx: &mut TestAppContext) {
        for (i, text) in [
            "schema_version = 2\n[appearance]\ntheme = \"dark\"\n",
            "[appearance\ntheme = 'dark'",
        ]
        .into_iter()
        .enumerate()
        {
            let (_dir, paths) = temp_paths();
            let file = write_settings(&paths, text);
            if i == 0 {
                start(cx, &paths);
            } else {
                let state = settings::load_from_paths(&paths);
                cx.update(|cx| cx.set_global(state));
            }
            assert!(cx.read_global::<SettingsState, _>(|s, _| s.save_blocked.is_some()));
            cx.update(appearance::toggle_mode);
            settle(cx);
            assert_eq!(read(&file), text);
        }
    }

    #[gpui::test]
    fn a_file_that_became_unwritable_is_reported(cx: &mut TestAppContext) {
        let (_dir, paths) = temp_paths();
        let file = write_settings(&paths, "schema_version = 1\n");
        start(cx, &paths);
        // Upgraded by a newer NineSQL while this one runs.
        assert!(std::fs::write(&file, "schema_version = 3\n").is_ok());
        cx.update(appearance::toggle_mode);
        settle(cx);
        assert_eq!(read(&file), "schema_version = 3\n");
        let problems = cx.read_global::<SettingsState, _>(|s, _| s.diagnostics.clone());
        assert!(
            problems.iter().any(|d| d.message().contains("not saved")),
            "{problems:?}"
        );
    }

    #[gpui::test]
    fn an_owned_symlink_is_written_through(cx: &mut TestAppContext) {
        let (dir, paths) = temp_paths();
        let dotfiles = dir.path().join("dotfiles");
        assert!(std::fs::create_dir_all(&dotfiles).is_ok());
        let target = dotfiles.join("ninesql.toml");
        assert!(
            std::fs::write(
                &target,
                "# managed by my dotfiles\n[appearance]\ntheme = \"dark\"\n"
            )
            .is_ok()
        );
        assert!(std::fs::create_dir_all(paths.config_dir()).is_ok());
        let link = paths.settings_file();
        assert!(std::os::unix::fs::symlink(&target, &link).is_ok());
        start(cx, &paths);
        let theme = cx.read_global::<SettingsState, _>(|s, _| s.settings.appearance.theme);
        assert_eq!(theme, ThemePreference::Dark);
        cx.update(appearance::toggle_mode);
        settle(cx);
        let link_meta = std::fs::symlink_metadata(&link);
        assert!(
            link_meta.is_ok_and(|m| m.file_type().is_symlink()),
            "the link stays"
        );
        let text = read(&target);
        assert!(text.contains("# managed by my dotfiles"), "{text}");
        assert!(text.contains("theme = \"light\""), "{text}");
    }
}

/// The X11 IME decoder in the graph is NineSQL's patched `xim-ctext` ([patch.crates-io]): the
/// preedit that crashed the spike (ibus, m17n `latn-post`, "é" as unescaped ISO 8859-1) and the
/// GB 2312 / KS C 5601 escapes upstream refused now decode.
#[test]
fn x11_ime_compound_text_decodes_without_error() {
    let decode = |bytes: &[u8]| xim_ctext::compound_text_to_utf8(bytes).ok();
    assert_eq!(decode(&[0xE9]).as_deref(), Some("é"));
    assert_eq!(decode(b"\x1b$)A\xd6\xd0").as_deref(), Some("中"));
    assert_eq!(decode(b"\x1b$)C\xb0\xa1").as_deref(), Some("가"));
    assert_eq!(decode(b"\x1b%G").as_deref(), Some(""));
}

/// GPUI's XIM client and its decoder resolve to NineSQL's patched copies under `crates/patches/`
/// (path sources in Cargo.lock, no registry `source`), at the exact versions the patch was made
/// from. Fails if a `cargo update` or a GPUI bump lets a crates.io release bypass the patch.
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
#[test]
fn x11_ime_crates_resolve_to_the_patched_copies() {
    use zed_xim as _;

    let lock_path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.lock");
    let lock = std::fs::read_to_string(lock_path)
        .unwrap_or_else(|e| panic!("cannot read the workspace Cargo.lock: {e}"));
    for (name, version) in [("xim-ctext", "0.3.0"), ("zed-xim", "0.4.0-zed")] {
        let entries: Vec<&str> = lock
            .split("[[package]]")
            .filter(|entry| entry.lines().any(|l| l == format!("name = \"{name}\"")))
            .collect();
        assert_eq!(
            entries.len(),
            1,
            "{name}: exactly one version in Cargo.lock"
        );
        let entry = entries[0];
        assert!(
            entry
                .lines()
                .any(|l| l == format!("version = \"{version}\"")),
            "{name}: Cargo.lock must keep {version}, the version patched under crates/patches/"
        );
        assert!(
            !entry.lines().any(|l| l.starts_with("source = ")),
            "{name}: Cargo.lock resolves it from a registry, so the patch is not applied"
        );
    }
}
