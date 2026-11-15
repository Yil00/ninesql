//! Application setup: settings, theme, key bindings, global actions and windows.

use std::rc::Rc;

use gpui_kit::component::TitleBar;
use gpui_kit::*;

use crate::actions::{
    DecreaseFontSize, IncreaseFontSize, NewWindow, Quit, ResetFontSize, ToggleTheme,
};
use crate::appearance;
use crate::keymap::EffectiveKeymap;
use crate::services::{Disconnected, Services};
use crate::settings::SettingsState;
use crate::workspace::Workspace;

/// Initial window size, logical pixels.
const WINDOW_SIZE: Size<Pixels> = Size {
    width: px(1280.),
    height: px(800.),
};

/// Initializes NineSQL on an application context: gpui-component, theme and fonts from the
/// settings, key bindings (defaults plus overrides) and the application-wide actions. Problems
/// found while installing the bindings are added to the settings problems.
pub fn init(mut settings: SettingsState, cx: &mut App) {
    gpui_kit::init(cx);
    cx.set_global(Services(Rc::new(Disconnected)));
    appearance::init(settings.settings.appearance.clone(), cx);
    let keymap = EffectiveKeymap::resolve(&settings.settings.keybindings);
    let problems = keymap.install(cx);
    for problem in &problems {
        tracing::warn!(problem = %problem, "key binding refused");
    }
    settings.diagnostics.extend(problems);
    cx.set_global(settings);
    // Changes still waiting for the debounce timer are written when NineSQL quits.
    cx.on_app_quit(crate::persist::flush_on_quit).detach();

    cx.on_action(|_: &Quit, cx| cx.quit());
    cx.on_action(|_: &NewWindow, cx| {
        open_window(cx);
    });
    cx.on_action(|_: &ToggleTheme, cx| appearance::toggle_mode(cx));
    cx.on_action(|_: &IncreaseFontSize, cx| appearance::zoom(Some(1.0), cx));
    cx.on_action(|_: &DecreaseFontSize, cx| appearance::zoom(Some(-1.0), cx));
    cx.on_action(|_: &ResetFontSize, cx| appearance::zoom(None, cx));
}

/// Runs an application-scope action (the command palette path). Returns false for any other
/// action.
pub fn perform_app_action(action: &dyn Action, cx: &mut App) -> bool {
    let any = action.as_any();
    if any.is::<Quit>() {
        cx.quit();
    } else if any.is::<NewWindow>() {
        open_window(cx);
    } else if any.is::<ToggleTheme>() {
        appearance::toggle_mode(cx);
    } else if any.is::<IncreaseFontSize>() {
        appearance::zoom(Some(1.0), cx);
    } else if any.is::<DecreaseFontSize>() {
        appearance::zoom(Some(-1.0), cx);
    } else if any.is::<ResetFontSize>() {
        appearance::zoom(None, cx);
    } else {
        return false;
    }
    true
}

/// Window options of a main window. The title bar is drawn by NineSQL only where the desktop
/// gives no server-side decorations (for example GNOME on Wayland).
#[must_use]
pub fn window_options(cx: &App) -> WindowOptions {
    let bounds = Bounds::centered(None, WINDOW_SIZE, cx);
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        window_min_size: Some(size(px(640.), px(400.))),
        app_id: Some("ninesql".to_owned()),
        titlebar: Some(TitlebarOptions {
            title: Some("NineSQL".into()),
            ..TitleBar::title_bar_options()
        }),
        ..TitleBar::window_options()
    }
}

/// Opens a main window with its own workspace. Errors are logged; NineSQL quits when no window
/// could ever be opened.
pub fn open_window(cx: &mut App) -> Option<(AnyWindowHandle, Entity<Workspace>)> {
    let options = window_options(cx);
    match gpui_kit::open_window(options, cx, |window, cx| {
        let workspace = cx.new(|cx| Workspace::new(window, cx));
        workspace.update(cx, |ws, cx| ws.focus_active(window, cx));
        workspace
    }) {
        Ok(opened) => Some(opened),
        Err(err) => {
            tracing::error!(error = %err, "cannot open a window");
            if cx.windows().is_empty() {
                cx.quit();
            }
            None
        }
    }
}

/// Entry point of the `ninesql` binary, with the drivers it ships registered in `registry`
/// (L5; src/main.rs).
pub fn run(registry: ninesql_core::DriverRegistry) {
    // Small local file reads before the event loop starts; no I/O happens on the foreground
    // thread once windows are open (I6).
    let settings = crate::settings::load_from_env();
    // INTEGRATION POINT (core, feat/p1-d-app-wiring): replace with
    // `CoreServices::start(CoreConfig::new(paths, project, registry))`, awaited on the GPUI
    // executor (it opens the workspace itself and assembles the secret stores, the vault, the
    // lock hooks and the desktop lock watcher), then build the hub from `CoreServices`.
    let hub_config = crate::connections::hub::assemble(registry);
    let app = gpui_kit::application().with_assets(gpui_kit::assets::Assets);
    app.run(move |cx| {
        init(settings, cx);
        crate::connections::hub::install(hub_config, cx);
        cx.on_app_quit(|cx| {
            if let Some(hub) = crate::connections::hub::Hub::get(cx) {
                hub.update(cx, |hub, _| hub.shutdown(std::time::Duration::from_secs(2)));
            }
            async {}
        })
        .detach();
        // Quit with the last window, as Linux desktop applications do.
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        open_window(cx);
        cx.activate(true);
    });
}
