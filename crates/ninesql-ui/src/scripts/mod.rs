//! Script files linked to a connection (F-SQL-11) and session restore (F-WS-06), wired into
//! the shell.
//!
//! - [`store`]: the project's `scripts/` folder and `scripts.json` on disk (blocking, run on
//!   GPUI's background executor, I6).
//! - [`session`]: `$XDG_STATE_HOME/ninesql/session.json` (references only, never editor text,
//!   I1 and ADR-0025 §4).
//! - Here: the "Open script…" picker, the "Save script as…" prompt, the unsaved-changes
//!   confirmations (closing a tab, a window, quitting), and the application-wide
//!   [`SessionState`]: which workspaces are open, the restore at start-up (no connection is
//!   opened, I3) and the write when NineSQL quits.

pub mod session;
pub mod store;

use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{ActiveTheme as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use ninesql_editor::script::ScriptPath;

use crate::connections::hub::{ConnectionHub, Hub};
use crate::workspace::Workspace;
use session::{RestorePlan, SessionFile, WindowSession};
use store::ScriptStore;

/// Most scripts shown at once in the picker (the filter narrows the list).
pub const PICKER_ROWS: usize = 200;

/// The script store of the hub's open project, `None` without a project.
#[must_use]
pub fn store_of(hub: &ConnectionHub) -> Option<ScriptStore> {
    let project = hub.project_paths()?;
    let roots = hub
        .manager()
        .map(|m| m.dbeaver_roots().clone())
        .unwrap_or_else(ninesql_core::DbeaverRoots::none);
    Some(ScriptStore::new(&project, roots))
}

/// A relative script path typed by the user: `.sql` is added when there is no extension.
///
/// # Errors
/// A user-facing reason.
pub fn parse_script_path(typed: &str) -> Result<ScriptPath, String> {
    let typed = typed.trim().trim_start_matches('/');
    if typed.is_empty() {
        return Err("Type a name, for example reports/monthly.sql.".to_owned());
    }
    let with_ext = if std::path::Path::new(typed).extension().is_none() {
        format!("{typed}.sql")
    } else {
        typed.to_owned()
    };
    ScriptPath::new(with_ext).map_err(|_| {
        "Use a relative .sql name inside the scripts folder: folders separated by /, no .. and no name starting with a dot.".to_owned()
    })
}

// --- dialogs -------------------------------------------------------------------------------

/// What the user chose about unsaved changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnsavedChoice {
    /// Save, then go on.
    Save,
    /// Go on and lose the changes.
    Discard,
}

/// Called with the user's choice (the dialog is closed first). Cancel calls nothing.
pub type OnUnsaved = Rc<dyn Fn(UnsavedChoice, &mut Window, &mut App)>;

/// Asks before unsaved text is lost: "Save" (when `can_save`), the destructive
/// `discard_label`, or Cancel (Escape, the close button).
pub fn open_unsaved_confirmation(
    title: &'static str,
    message: String,
    can_save: bool,
    discard_label: &'static str,
    on_choice: OnUnsaved,
    window: &mut Window,
    cx: &mut App,
) {
    window.open_dialog(cx, move |dialog, _window, _cx| {
        let discard = {
            let on_choice = on_choice.clone();
            Button::new("unsaved-discard")
                .danger()
                .label(discard_label)
                .on_click(move |_, window, cx| {
                    window.close_dialog(cx);
                    on_choice(UnsavedChoice::Discard, window, cx);
                })
        };
        let save = can_save.then(|| {
            let on_choice = on_choice.clone();
            Button::new("unsaved-save")
                .primary()
                .label("Save")
                .on_click(move |_, window, cx| {
                    window.close_dialog(cx);
                    on_choice(UnsavedChoice::Save, window, cx);
                })
        });
        let cancel = Button::new("unsaved-cancel")
            .label("Cancel")
            .on_click(|_, window, cx| window.close_dialog(cx));
        dialog
            .title(title)
            .w(px(520.))
            .child(div().text_sm().child(message.clone()))
            .footer(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(cancel)
                    .child(discard)
                    .children(save),
            )
    });
}

/// Called with the script picked in "Open script…".
pub type OnPick = Rc<dyn Fn(ScriptPath, &mut Window, &mut App)>;

/// "Open script…": the scripts of the project's `scripts/` (and only those), with a filter.
pub struct ScriptPicker {
    scripts: Vec<ScriptPath>,
    truncated: bool,
    filter: Entity<InputState>,
    selected: usize,
    on_pick: OnPick,
    _subscriptions: Vec<Subscription>,
}

impl ScriptPicker {
    /// A picker over `scripts` (`truncated`: the listing stopped at its limit).
    pub fn new(
        scripts: Vec<ScriptPath>,
        truncated: bool,
        on_pick: OnPick,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let filter = cx
            .new(|cx| InputState::new(window, cx).placeholder("Filter scripts by name or folder"));
        let subscriptions = vec![cx.subscribe_in(
            &filter,
            window,
            |this: &mut Self, _, event: &InputEvent, window, cx| match event {
                InputEvent::Change => {
                    this.selected = 0;
                    cx.notify();
                }
                InputEvent::PressEnter { .. } => this.pick(window, cx),
                _ => {}
            },
        )];
        Self {
            scripts,
            truncated,
            filter,
            selected: 0,
            on_pick,
            _subscriptions: subscriptions,
        }
    }

    /// The filter input.
    #[must_use]
    pub fn filter(&self) -> &Entity<InputState> {
        &self.filter
    }

    /// The scripts matching the filter (case-insensitive substring of the path), bounded.
    #[must_use]
    pub fn matches(&self, cx: &App) -> Vec<ScriptPath> {
        let needle = self.filter.read(cx).value().trim().to_lowercase();
        self.scripts
            .iter()
            .filter(|s| {
                needle.is_empty()
                    || s.as_path()
                        .to_string_lossy()
                        .to_lowercase()
                        .contains(&needle)
            })
            .take(PICKER_ROWS)
            .cloned()
            .collect()
    }

    /// Selects match `ix`.
    pub fn select(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.selected = ix;
        cx.notify();
    }

    /// Opens the selected match.
    pub fn pick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(script) = self.matches(cx).get(self.selected).cloned() else {
            return;
        };
        window.close_dialog(cx);
        (self.on_pick)(script, window, cx);
    }

    fn on_key(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let len = self.matches(cx).len();
        match event.keystroke.key.as_str() {
            "down" if len > 0 => {
                self.select((self.selected + 1).min(len - 1), cx);
                cx.stop_propagation();
            }
            "up" if len > 0 => {
                self.select(self.selected.saturating_sub(1), cx);
                cx.stop_propagation();
            }
            _ => {}
        }
    }
}

impl Render for ScriptPicker {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let matches = self.matches(cx);
        let theme = cx.theme();
        let muted = crate::appearance::ensure_contrast(
            theme.muted_foreground,
            &[theme.background],
            crate::appearance::MIN_TEXT_CONTRAST,
        );
        let active = theme.list_active;
        let rows = matches.iter().enumerate().map(|(ix, script)| {
            let path = crate::browse::display_name(&script.as_path().to_string_lossy());
            h_flex()
                .id(("script-pick", ix))
                .w_full()
                .px_2()
                .py_1()
                .rounded_sm()
                .text_sm()
                .when(ix == self.selected, |el| el.bg(active))
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.select(ix, cx);
                    this.pick(window, cx);
                }))
                .child(path)
        });
        let status = if self.scripts.is_empty() {
            "No script yet: \"New script\" or \"Save script\" creates one in the scripts folder."
                .to_owned()
        } else if matches.is_empty() {
            "No script matches.".to_owned()
        } else {
            let mut s = format!("{} of {} scripts", matches.len(), self.scripts.len());
            if self.truncated {
                s.push_str(" (the listing stopped at its limit)");
            }
            s
        };
        v_flex()
            .id("script-picker")
            .gap_2()
            .capture_key_down(cx.listener(Self::on_key))
            .child(Input::new(&self.filter))
            .child(div().text_xs().text_color(muted).child(status))
            .child(
                v_flex()
                    .id("script-picker-rows")
                    .max_h(px(360.))
                    .overflow_y_scroll()
                    .children(rows),
            )
    }
}

/// Opens "Open script…" over `window`.
pub fn open_script_picker(
    scripts: Vec<ScriptPath>,
    truncated: bool,
    on_pick: OnPick,
    window: &mut Window,
    cx: &mut App,
) -> Entity<ScriptPicker> {
    let view = cx.new(|cx| ScriptPicker::new(scripts, truncated, on_pick, window, cx));
    let shown = view.clone();
    let first = std::cell::Cell::new(true);
    window.open_dialog(cx, move |dialog, window, cx| {
        if first.replace(false) {
            let input = shown.read(cx).filter.clone();
            input.update(cx, |s, cx| s.focus(window, cx));
        }
        dialog.title("Open script").w(px(560.)).child(shown.clone())
    });
    view
}

/// Called with the name typed in "Save script as…".
pub type OnName = Rc<dyn Fn(ScriptPath, &mut Window, &mut App)>;

/// "Save script as…": a relative name inside the scripts folder.
pub struct SaveAsPrompt {
    name: Entity<InputState>,
    error: Option<String>,
    on_name: OnName,
    _subscriptions: Vec<Subscription>,
}

impl SaveAsPrompt {
    /// A prompt prefilled with `initial`.
    pub fn new(
        initial: &str,
        on_name: OnName,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let initial = initial.to_owned();
        let name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("reports/monthly.sql")
                .default_value(initial)
        });
        let subscriptions = vec![cx.subscribe_in(
            &name,
            window,
            |this: &mut Self, _, event: &InputEvent, window, cx| match event {
                InputEvent::Change => {
                    this.error = None;
                    cx.notify();
                }
                InputEvent::PressEnter { .. } => this.submit(window, cx),
                _ => {}
            },
        )];
        Self {
            name,
            error: None,
            on_name,
            _subscriptions: subscriptions,
        }
    }

    /// The name input.
    #[must_use]
    pub fn input(&self) -> &Entity<InputState> {
        &self.name
    }

    /// The last validation error.
    #[must_use]
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Validates the name and hands it over (the dialog closes), or shows why it is refused.
    pub fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let typed = self.name.read(cx).value().to_string();
        match parse_script_path(&typed) {
            Ok(path) => {
                window.close_dialog(cx);
                (self.on_name)(path, window, cx);
            }
            Err(e) => {
                self.error = Some(e);
                cx.notify();
            }
        }
    }
}

impl Render for SaveAsPrompt {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let danger = crate::appearance::ensure_contrast(
            theme.danger,
            &[theme.background],
            crate::appearance::MIN_TEXT_CONTRAST,
        );
        let muted = crate::appearance::ensure_contrast(
            theme.muted_foreground,
            &[theme.background],
            crate::appearance::MIN_TEXT_CONTRAST,
        );
        v_flex()
            .gap_2()
            .text_sm()
            .child(
                div().text_color(muted).child(
                    "Name inside the project's scripts folder; folders are created as needed.",
                ),
            )
            .child(Input::new(&self.name))
            .children(
                self.error
                    .clone()
                    .map(|e| div().text_color(danger).child(e)),
            )
            .child(
                h_flex().justify_end().child(
                    Button::new("save-as-save")
                        .primary()
                        .label("Save")
                        .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
                ),
            )
    }
}

/// Opens "Save script as…" over `window`.
pub fn open_save_as(
    initial: &str,
    on_name: OnName,
    window: &mut Window,
    cx: &mut App,
) -> Entity<SaveAsPrompt> {
    let view = cx.new(|cx| SaveAsPrompt::new(initial, on_name, window, cx));
    let shown = view.clone();
    let first = std::cell::Cell::new(true);
    window.open_dialog(cx, move |dialog, window, cx| {
        if first.replace(false) {
            let input = shown.read(cx).name.clone();
            input.update(cx, |s, cx| s.focus(window, cx));
        }
        dialog
            .title("Save script as")
            .w(px(520.))
            .child(shown.clone())
    });
    view
}

// --- session state ---------------------------------------------------------------------------

/// The application's session state (a GPUI global).
#[derive(Default)]
pub struct SessionState {
    /// Open workspaces, in the order their windows opened.
    workspaces: Vec<WeakEntity<Workspace>>,
    /// The snapshot of the last window closed (closing it quits NineSQL).
    last_closed: Option<WindowSession>,
    /// Write the session file at quit (the application turns it on; tests call the parts).
    enabled: bool,
    /// The restore at start-up ran: the file may be written at quit (it is never overwritten
    /// before the previous session was read).
    restored: bool,
    /// The file must be kept as it is (written by a newer NineSQL).
    keep_file: bool,
    /// Quitting with unsaved changes was confirmed.
    quit_confirmed: bool,
    /// Waits for the hub to start before restoring.
    _hub_observer: Option<Subscription>,
}

impl Global for SessionState {}

impl SessionState {
    /// Records an open workspace.
    pub fn register(workspace: WeakEntity<Workspace>, cx: &mut App) {
        let state = cx.default_global::<Self>();
        state.workspaces.retain(|w| w.upgrade().is_some());
        state.workspaces.push(workspace);
    }

    /// The open workspaces, in window order.
    #[must_use]
    pub fn workspaces(cx: &App) -> Vec<Entity<Workspace>> {
        cx.try_global::<Self>()
            .map(|s| {
                s.workspaces
                    .iter()
                    .filter_map(WeakEntity::upgrade)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// A window is about to close with `snapshot` as its session: kept when it is the last
    /// window (closing it quits NineSQL); the workspace is forgotten.
    pub fn window_closing(workspace: &Entity<Workspace>, snapshot: WindowSession, cx: &mut App) {
        let id = workspace.entity_id();
        let state = cx.default_global::<Self>();
        state
            .workspaces
            .retain(|w| w.upgrade().is_some_and(|e| e.entity_id() != id));
        if state.workspaces.is_empty() {
            state.last_closed = Some(snapshot);
        }
    }

    /// Whether quitting was confirmed despite unsaved changes.
    #[must_use]
    pub fn quit_confirmed(cx: &App) -> bool {
        cx.try_global::<Self>().is_some_and(|s| s.quit_confirmed)
    }

    /// Marks the start-up restore as done (`keep_file`: never overwrite the file).
    pub fn mark_restored(keep_file: bool, cx: &mut App) {
        let state = cx.default_global::<Self>();
        state.restored = true;
        state.keep_file |= keep_file;
    }

    /// The session to write now: every open window, else the last one closed. `None` when
    /// nothing may be written (restore not done, a newer file, no project).
    #[must_use]
    pub fn current(cx: &App) -> Option<SessionFile> {
        let state = cx.try_global::<Self>()?;
        if !state.restored || state.keep_file {
            return None;
        }
        let hub = Hub::get(cx)?;
        let project = hub.read(cx).project_name()?;
        let live = Self::workspaces(cx);
        let windows: Vec<WindowSession> = if live.is_empty() {
            state.last_closed.clone().into_iter().collect()
        } else {
            live.iter()
                .map(|w| w.read(cx).session_snapshot(cx))
                .collect()
        };
        Some(SessionFile { project, windows })
    }
}

/// Turns session restore on for the application: restores the previous session once the hub
/// has started, and writes the session when NineSQL quits.
pub fn install(cx: &mut App) {
    cx.default_global::<SessionState>().enabled = true;
    cx.on_app_quit(write_on_quit).detach();
    let Some(hub) = Hub::get(cx) else {
        return;
    };
    if !hub.read(cx).is_starting() {
        start_restore(cx);
        return;
    }
    let subscription = cx.observe(&hub, |hub, cx| {
        if hub.read(cx).is_starting() {
            return;
        }
        // Once: the first notification after the start takes the observer away.
        cx.defer(|cx| {
            let Some(observer) = cx.default_global::<SessionState>()._hub_observer.take() else {
                return;
            };
            drop(observer);
            start_restore(cx);
        });
    });
    cx.default_global::<SessionState>()._hub_observer = Some(subscription);
}

/// Reads the previous session off the UI thread and reopens it (no connection is opened).
pub fn start_restore(cx: &mut App) {
    let Some(hub) = Hub::get(cx) else {
        return;
    };
    let (store, path, project) = {
        let hub = hub.read(cx);
        (store_of(hub), hub.session_file(), hub.project_name())
    };
    let (Some(store), Some(path), Some(project)) = (store, path, project) else {
        // No project: nothing to restore, and nothing is written at quit (no `restored`).
        return;
    };
    let read = cx.background_spawn(async move { session::read_plan(&path, &store, &project) });
    cx.spawn(async move |cx| {
        let plan = read.await;
        cx.update(|cx| apply_plan(plan, cx));
    })
    .detach();
}

/// Reopens the windows of `plan`: the first in the first open window, the others in new
/// windows. A notice lists what was skipped.
pub fn apply_plan(plan: RestorePlan, cx: &mut App) {
    SessionState::mark_restored(plan.keep_file, cx);
    let mut first_window = None;
    for (i, window_plan) in plan.windows.into_iter().enumerate() {
        let target = if i == 0 {
            SessionState::workspaces(cx).into_iter().next()
        } else {
            None
        };
        let target = match target {
            Some(t) => Some(t),
            None => crate::app::open_window(cx).map(|(_, w)| w),
        };
        let Some(workspace) = target else {
            continue;
        };
        let handle = workspace.read(cx).window_handle();
        let _ = handle.update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| ws.apply_restore(window_plan, window, cx));
        });
        first_window.get_or_insert(handle);
    }
    if plan.notices.is_empty() {
        return;
    }
    let handle = first_window.or_else(|| {
        SessionState::workspaces(cx)
            .first()
            .map(|w| w.read(cx).window_handle())
    });
    if let Some(handle) = handle {
        let text = plan.notices.join("\n");
        let _ = handle.update(cx, |_, window, cx| {
            window.push_notification(
                Notification::warning(text)
                    .title("Previous session")
                    .autohide(false),
                cx,
            );
        });
    }
}

/// The quit handler: writes the session file on a background thread (bounded by GPUI's quit
/// timeout).
fn write_on_quit(cx: &mut App) -> impl std::future::Future<Output = ()> + use<> {
    let enabled = cx.try_global::<SessionState>().is_some_and(|s| s.enabled);
    let job = enabled
        .then(|| {
            let file = SessionState::current(cx)?;
            let hub = Hub::get(cx)?;
            let hub = hub.read(cx);
            Some((hub.session_file()?, store_of(hub), file))
        })
        .flatten();
    let task = job.map(|(path, store, file)| {
        cx.background_executor()
            .spawn(async move { session::write(&path, store.as_ref(), file) })
    });
    async move {
        if let Some(task) = task
            && let Err(problem) = task.await
        {
            tracing::warn!(problem = %problem, "session not saved at quit");
        }
    }
}

/// "Quit NineSQL": asks first when a window holds unsaved text.
pub fn request_quit(cx: &mut App) {
    if !SessionState::quit_confirmed(cx) {
        let dirty: Vec<(Entity<Workspace>, usize)> = SessionState::workspaces(cx)
            .into_iter()
            .map(|w| {
                let n = w.read(cx).dirty_tab_count(cx);
                (w, n)
            })
            .filter(|(_, n)| *n > 0)
            .collect();
        let total: usize = dirty.iter().map(|(_, n)| n).sum();
        if let Some((workspace, _)) = dirty.into_iter().next() {
            let handle = workspace.read(cx).window_handle();
            let _ = handle.update(cx, |_, window, cx| {
                open_unsaved_confirmation(
                    "Unsaved scripts",
                    format!(
                        "{total} editor tab(s) hold unsaved text, which is lost on quit (unsaved text is never kept for the next start)."
                    ),
                    false,
                    "Quit without saving",
                    Rc::new(|_, _, cx| {
                        cx.default_global::<SessionState>().quit_confirmed = true;
                        cx.quit();
                    }),
                    window,
                    cx,
                );
            });
            return;
        }
    }
    cx.quit();
}

#[cfg(test)]
mod tests;
