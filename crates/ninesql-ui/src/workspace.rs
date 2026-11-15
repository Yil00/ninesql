//! The main window content (F-WS-01): navigator dock, editor area with tabs and splits, status
//! bar, and on desktops without server-side decorations a client-side title bar.

use std::collections::HashMap;
use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::resizable::{h_resizable, resizable_panel, v_resizable};
use gpui_kit::component::status_bar::StatusBar;
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::{
    ActiveTheme as _, IconName, Sizable as _, TitleBar, WindowExt as _, h_flex, v_flex,
    window_border,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::actions::{
    self, CloseTab, CloseWindow, CommandPalette, ConnectConnection, DeleteConnection,
    DisconnectConnection, DuplicateConnection, EditConnection, EnableConnection, FocusNavigator,
    FocusNextPane, FocusPreviousPane, ImportFromDbeaver, ImportFromDbeaverFolder, LockSecrets,
    NewConnection, NewEditorTab, NextTab, PreviousTab, Scope, ShowConnectionProblems,
    ShowSettingsProblems, SplitDown, SplitRight, TestConnection, ToggleNavigator,
    WORKSPACE_CONTEXT,
};
use crate::appearance;
use crate::connections::connect_flow::{ConnectFlow, FlowFinished, FlowResult, Purpose};
use crate::connections::dialog::{
    ConnectionDialog, ConnectionDialogCancelled, ConnectionSaved, new_form,
};
use crate::connections::form::ConnectionForm;
use crate::connections::hub::DetectedDbeaver;
use crate::connections::hub::{ConnectionHub, HubEvent, hub_or_install_empty, lock_reason_text};
use crate::connections::visuals::{ConnectionBadges, badges};
use crate::editor_tab::SqlEditorTab;
use crate::import::enable::{ConnectionEnabled, disabled_text, open_enable_prompt};
use crate::import::flow::{ImportFlow, open_import_dialog};
use crate::keymap::EffectiveKeymap;
use crate::layout::{EditorLayout, Node, PaneId, SplitAxis, TabId};
use crate::navigator::Navigator;
use crate::services::Services;
use crate::settings::SettingsState;
use gpui_kit::component::notification::Notification;
use ninesql_core::ConnectionId;

/// Width of the navigator dock when the window opens.
const NAVIGATOR_WIDTH: Pixels = px(260.);

/// The workspace view of one window.
pub struct Workspace {
    focus: FocusHandle,
    layout: EditorLayout,
    tabs: HashMap<TabId, Entity<SqlEditorTab>>,
    navigator: Entity<Navigator>,
    navigator_visible: bool,
    next_script: usize,
    hub: Entity<ConnectionHub>,
    flow: Entity<ConnectFlow>,
    active_connection: Option<ConnectionId>,
    dialog: Option<Entity<ConnectionDialog>>,
    import: Option<Entity<ImportFlow>>,
    first_run_hidden: bool,
    _subscriptions: Vec<Subscription>,
}

impl Workspace {
    /// A workspace with the navigator and one empty SQL editor.
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let hub = hub_or_install_empty(cx);
        let flow = cx.new(|_| ConnectFlow::new(hub.clone()));
        let subscriptions = vec![
            cx.observe(&hub, |_, _, cx| cx.notify()),
            cx.subscribe_in(&flow, window, Self::on_flow_finished),
            cx.subscribe_in(&hub, window, Self::on_hub_event),
        ];
        let mut this = Self {
            focus: cx.focus_handle(),
            layout: EditorLayout::new(),
            tabs: HashMap::new(),
            navigator: cx.new(Navigator::new),
            navigator_visible: true,
            next_script: 1,
            hub,
            flow,
            active_connection: None,
            dialog: None,
            import: None,
            first_run_hidden: false,
            _subscriptions: subscriptions,
        };
        this.new_tab(window, cx);
        this
    }

    /// The tab and split model.
    #[must_use]
    pub fn layout(&self) -> &EditorLayout {
        &self.layout
    }

    /// The editor of a tab.
    #[must_use]
    pub fn editor(&self, tab: TabId) -> Option<&Entity<SqlEditorTab>> {
        self.tabs.get(&tab)
    }

    /// Titles of all tabs, in pane order.
    #[must_use]
    pub fn tab_titles(&self, cx: &App) -> Vec<SharedString> {
        self.layout
            .all_tabs()
            .into_iter()
            .filter_map(|t| self.tabs.get(&t))
            .map(|e| e.read(cx).title().clone())
            .collect()
    }

    /// Title of the active tab.
    #[must_use]
    pub fn active_title(&self, cx: &App) -> Option<SharedString> {
        let tab = self.layout.active_tab()?;
        self.tabs.get(&tab).map(|e| e.read(cx).title().clone())
    }

    /// Whether the navigator dock is shown.
    #[must_use]
    pub fn navigator_visible(&self) -> bool {
        self.navigator_visible
    }

    /// The navigator view.
    #[must_use]
    pub fn navigator(&self) -> &Entity<Navigator> {
        &self.navigator
    }

    fn create_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) -> TabId {
        let id = self.layout.allocate_tab();
        let title = format!("Script-{}", self.next_script);
        self.next_script += 1;
        let editor = cx.new(|cx| SqlEditorTab::new(title, window, cx));
        self.tabs.insert(id, editor);
        id
    }

    /// Opens an empty SQL editor in the active pane.
    pub fn new_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) -> TabId {
        let id = self.create_tab(window, cx);
        self.layout.open_tab(id);
        self.after_change(window, cx);
        id
    }

    /// Closes a tab.
    pub fn close_tab(&mut self, tab: TabId, window: &mut Window, cx: &mut Context<Self>) {
        if self.layout.close_tab(tab) {
            self.tabs.remove(&tab);
            self.after_change(window, cx);
        }
    }

    /// Closes the active tab.
    pub fn close_active_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(tab) = self.layout.active_tab() {
            self.close_tab(tab, window, cx);
        }
    }

    /// Activates a tab and its pane.
    pub fn activate_tab(&mut self, tab: TabId, window: &mut Window, cx: &mut Context<Self>) {
        if self.layout.activate_tab(tab) {
            self.after_change(window, cx);
        }
    }

    /// Next or previous tab of the active pane.
    pub fn cycle_tab(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.layout.cycle_tab(forward).is_some() {
            self.after_change(window, cx);
        }
    }

    /// Splits the active pane, with a new empty editor in the new pane.
    pub fn split(&mut self, axis: SplitAxis, window: &mut Window, cx: &mut Context<Self>) {
        let tab = self.create_tab(window, cx);
        self.layout.split(axis, tab);
        self.after_change(window, cx);
    }

    /// Next or previous pane.
    pub fn cycle_pane(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.layout.cycle_pane(forward);
        self.after_change(window, cx);
    }

    fn activate_pane(&mut self, pane: PaneId, cx: &mut Context<Self>) {
        if self.layout.active_pane_id() != pane && self.layout.activate_pane(pane) {
            cx.notify();
        }
    }

    /// Shows or hides the navigator dock.
    pub fn toggle_navigator(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.navigator_visible = !self.navigator_visible;
        if !self.navigator_visible && self.navigator.focus_handle(cx).contains_focused(window, cx) {
            self.focus_active(window, cx);
        }
        cx.notify();
    }

    /// Shows the navigator dock and focuses it.
    pub fn focus_navigator(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.navigator_visible = true;
        self.navigator.update(cx, |n, cx| n.focus_tree(window, cx));
        cx.notify();
    }

    /// The application's connections.
    #[must_use]
    pub fn hub(&self) -> &Entity<ConnectionHub> {
        &self.hub
    }

    /// The connect flow of this window.
    #[must_use]
    pub fn flow(&self) -> &Entity<ConnectFlow> {
        &self.flow
    }

    /// The connection dialog, while it is open.
    #[must_use]
    pub fn connection_dialog(&self) -> Option<&Entity<ConnectionDialog>> {
        self.dialog.as_ref()
    }

    /// The connection of the last session opened in this window (status bar, frame color).
    /// Editor tabs get their own connection in P3.
    #[must_use]
    pub fn active_connection(&self) -> Option<ConnectionId> {
        self.active_connection
    }

    fn selected_connection(&self, cx: &App) -> Option<ConnectionId> {
        self.navigator.read(cx).selected_connection(cx)
    }

    fn notify_no_selection(window: &mut Window, cx: &mut App) {
        window.push_notification(
            Notification::info("Select a connection in the navigator first."),
            cx,
        );
    }

    fn selected(&self, cx: &App) -> Option<ninesql_core::Connection> {
        self.selected_connection(cx)
            .and_then(|id| self.hub.read(cx).connection(id).cloned())
    }

    /// Opens the connection dialog on `form`.
    pub fn open_connection_dialog(
        &mut self,
        form: ConnectionForm,
        title: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let hub = self.hub.clone();
        let flow = self.flow.clone();
        let dialog = cx.new(|cx| ConnectionDialog::new(hub, flow, form, window, cx));
        let sub = cx.subscribe_in(
            &dialog,
            window,
            |this: &mut Self, _, saved: &ConnectionSaved, window, cx| {
                let id = saved.0;
                this.dialog = None;
                window.close_dialog(cx);
                this.navigator
                    .update(cx, |n, cx| n.select_connection(id, cx));
                let name = this
                    .hub
                    .read(cx)
                    .connection(id)
                    .map(|c| c.name.clone())
                    .unwrap_or_default();
                window.push_notification(Notification::success(format!("Saved \"{name}\".")), cx);
                if this.hub.read(cx).is_open(id) {
                    // The open sessions keep the settings they were opened with (policy,
                    // read-only, TLS, SSH): say so instead of implying the change is live.
                    window.push_notification(
                        Notification::info(format!(
                            "\"{name}\" is connected: its open sessions keep the previous settings until you reconnect (Disconnect, then Connect)."
                        ))
                        .autohide(false),
                        cx,
                    );
                }
            },
        );
        self._subscriptions.push(sub);
        let cancelled = cx.subscribe(
            &dialog,
            |this: &mut Self, _, _: &ConnectionDialogCancelled, _| {
                this.dialog = None;
            },
        );
        self._subscriptions.push(cancelled);
        self.dialog = Some(dialog.clone());
        let first = std::cell::Cell::new(true);
        let weak = cx.entity().downgrade();
        window.open_dialog(cx, move |d, window, cx| {
            if first.replace(false)
                && let Some(name) = dialog.read(cx).input("name").cloned()
            {
                name.focus_handle(cx).focus(window, cx);
            }
            let weak = weak.clone();
            d.title(title)
                .w(px(760.))
                .on_close(move |_, window, cx| {
                    // Escape, the close button or a click outside: no typed secret survives.
                    let dialog = weak.update(cx, |ws, _| ws.dialog.take()).ok().flatten();
                    if let Some(dialog) = dialog {
                        dialog.update(cx, |d, cx| d.clear_secret_inputs(window, cx));
                    }
                })
                .child(dialog.clone())
        });
    }

    /// "New connection".
    pub fn new_connection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match new_form(self.hub.read(cx)) {
            Some(form) => self.open_connection_dialog(form, "New connection", window, cx),
            None => window.push_notification(
                Notification::error("No database driver is available in this build."),
                cx,
            ),
        }
    }

    /// "Edit connection" on the navigator's selection.
    pub fn edit_connection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(c) = self.selected(cx) else {
            return Self::notify_no_selection(window, cx);
        };
        self.open_connection_dialog(
            ConnectionForm::from_connection(&c),
            "Edit connection",
            window,
            cx,
        );
    }

    /// "Duplicate connection": the dialog opens on a copy; saved secrets are not copied.
    pub fn duplicate_connection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(c) = self.selected(cx) else {
            return Self::notify_no_selection(window, cx);
        };
        self.open_connection_dialog(
            ConnectionForm::duplicate_of(&c),
            "Duplicate connection",
            window,
            cx,
        );
    }

    /// "Delete connection": asks first, then deletes it and its saved secrets.
    pub fn delete_connection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(c) = self.selected(cx) else {
            return Self::notify_no_selection(window, cx);
        };
        let this = cx.entity().downgrade();
        let id = c.id;
        let name: SharedString = c.name.clone().into();
        let secrets = c.secret_refs().len();
        let open = self.hub.read(cx).is_open(id);
        window.open_dialog(cx, move |dialog, _window, cx| {
            let this = this.clone();
            let delete = Button::new("delete-confirm")
                .danger()
                .label("Delete")
                .on_click(move |_, window, cx| {
                    window.close_dialog(cx);
                    let _ = this.update(cx, |ws, cx| ws.delete_connection_now(id, window, cx));
                });
            let cancel = Button::new("delete-cancel")
                .label("Cancel")
                .on_click(|_, window, cx| window.close_dialog(cx));
            let muted = cx.theme().muted_foreground;
            dialog
                .title("Delete connection")
                .w(px(480.))
                .child(
                    v_flex()
                        .gap_2()
                        .text_sm()
                        .child(format!("Delete \"{name}\"? This cannot be undone."))
                        .when(secrets > 0, |el| {
                            el.child(div().text_color(muted).child(format!(
                                "Its {secrets} saved credential(s) are deleted from the keyring or vault."
                            )))
                        })
                        .when(open, |el| {
                            el.child(div().text_color(muted).child("Its open sessions are closed."))
                        }),
                )
                .footer(h_flex().gap_2().justify_end().child(cancel).child(delete))
        });
    }

    /// Deletes a connection without asking (the confirmed path of "Delete connection").
    pub fn delete_connection_now(
        &mut self,
        id: ConnectionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.active_connection == Some(id) {
            self.active_connection = None;
        }
        let task = self.hub.update(cx, |hub, cx| hub.remove(id, cx));
        cx.spawn_in(window, async move |_, cx| {
            if let Err(e) = task.await {
                let _ = cx.update(|window, cx| {
                    window.push_notification(Notification::error(format!("Not deleted: {e}")), cx);
                });
            }
        })
        .detach();
    }

    /// Opens (or tests) the navigator's selection.
    pub fn connect_selected(
        &mut self,
        purpose: Purpose,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(id) = self.selected_connection(cx) else {
            return Self::notify_no_selection(window, cx);
        };
        self.connect(id, purpose, window, cx);
    }

    /// Opens (or tests) a saved connection through the connect flow.
    pub fn connect(
        &mut self,
        id: ConnectionId,
        purpose: Purpose,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(c) = self.hub.read(cx).connection(id).cloned() else {
            return;
        };
        if let Some(why) = disabled_text(c.enabled, c.disabled_reason.as_ref()) {
            // The core refuses a disabled connection too; say why, and how to enable it.
            window.push_notification(
                Notification::warning(format!("{why} Use \"Enable connection\" first."))
                    .title(format!("\"{}\" is disabled", c.name)),
                cx,
            );
            return;
        }
        if self.flow.read(cx).is_busy() {
            window.push_notification(
                Notification::info("A connection is already being opened."),
                cx,
            );
            return;
        }
        self.flow.update(cx, |flow, cx| {
            flow.start(c, purpose, Vec::new(), window, cx)
        });
    }

    /// Closes the sessions of the navigator's selection.
    pub fn disconnect_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.selected_connection(cx) else {
            return Self::notify_no_selection(window, cx);
        };
        self.hub.update(cx, |hub, cx| hub.disconnect(id, cx));
        if self.active_connection == Some(id) {
            self.active_connection = None;
        }
        cx.notify();
    }

    fn on_flow_finished(
        &mut self,
        _: &Entity<ConnectFlow>,
        event: &FlowFinished,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Tests run from the dialog report inside it.
        let from_dialog = event.purpose == Purpose::Test
            && (self.dialog.is_some()
                || self
                    .import
                    .as_ref()
                    .is_some_and(|f| f.read(cx).is_testing()));
        match &event.result {
            FlowResult::Opened {
                connection,
                warnings,
            } => {
                self.active_connection = Some(*connection);
                let name = self
                    .hub
                    .read(cx)
                    .connection(*connection)
                    .map(|c| c.name.clone())
                    .unwrap_or_default();
                window.push_notification(
                    Notification::success(format!("Connected to \"{name}\".")),
                    cx,
                );
                if !warnings.is_empty() {
                    window.push_notification(
                        Notification::warning(warnings.join("\n"))
                            .title("Security warnings")
                            .autohide(false),
                        cx,
                    );
                }
            }
            FlowResult::TestOk {
                server_version,
                warnings,
            } if !from_dialog => {
                let version = server_version
                    .clone()
                    .unwrap_or_else(|| "the server".to_owned());
                window.push_notification(
                    Notification::success(format!("Connected to {version}; session closed.")),
                    cx,
                );
                if !warnings.is_empty() {
                    window.push_notification(
                        Notification::warning(warnings.join("\n"))
                            .title("Security warnings")
                            .autohide(false),
                        cx,
                    );
                }
            }
            FlowResult::Failed(e) if !from_dialog => {
                window.push_notification(
                    Notification::error(e.clone()).title("Connection failed"),
                    cx,
                );
            }
            _ => {}
        }
        cx.notify();
    }

    /// The DBeaver import flow of this window, once opened.
    #[must_use]
    pub fn import_flow(&self) -> Option<&Entity<ImportFlow>> {
        self.import.as_ref()
    }

    fn open_import(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Entity<ImportFlow> {
        let flow = match &self.import {
            Some(f) if f.read(cx).is_busy() => f.clone(),
            _ => {
                let hub = self.hub.clone();
                let connect = self.flow.clone();
                let flow = cx.new(|cx| ImportFlow::new(hub, connect, window, cx));
                self.import = Some(flow.clone());
                flow
            }
        };
        open_import_dialog(flow.clone(), window, cx);
        flow
    }

    /// Opens the import screen waiting for a folder (no picker), and returns its flow.
    pub fn open_import_screen(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<ImportFlow> {
        let flow = self.open_import(window, cx);
        flow.update(cx, |f, cx| f.choose_folder_stage(None, cx));
        flow
    }

    /// "Import from DBeaver": previews the workspaces of the known locations (P2-B).
    pub fn import_from_dbeaver(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.import.as_ref().is_some_and(|f| f.read(cx).is_busy()) {
            window.push_notification(Notification::info("An import is already running."), cx);
            return;
        }
        let flow = self.open_import(window, cx);
        flow.update(cx, |f, cx| f.start_detect(window, cx));
    }

    /// "Import from DBeaver: choose folder": the folder picker, then the preview.
    pub fn import_from_dbeaver_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.import.as_ref().is_some_and(|f| f.read(cx).is_busy()) {
            window.push_notification(Notification::info("An import is already running."), cx);
            return;
        }
        let flow = self.open_import(window, cx);
        flow.update(cx, |f, cx| {
            f.choose_folder_stage(None, cx);
            f.choose_folder(window, cx);
        });
    }

    /// The first-run import offer (P2-B): shown while the project has no connection, a DBeaver
    /// workspace was found, `[import] first_run_prompt` is on and it was not hidden in this
    /// window.
    #[must_use]
    pub fn first_run_offer(&self, cx: &App) -> Option<Vec<DetectedDbeaver>> {
        if self.first_run_hidden {
            return None;
        }
        let enabled = cx
            .try_global::<SettingsState>()
            .is_none_or(|s| s.settings.import.first_run_prompt);
        let hub = self.hub.read(cx);
        if !enabled || hub.is_starting() || !hub.connections().is_empty() {
            return None;
        }
        hub.detected_dbeaver()
            .filter(|found| !found.is_empty())
            .map(<[DetectedDbeaver]>::to_vec)
    }

    /// Hides the first-run offer: for this window ("Not now"), or for good ("Don't show
    /// again", saved as `[import] first_run_prompt = false` in settings.toml).
    pub fn dismiss_first_run(&mut self, remember: bool, cx: &mut Context<Self>) {
        self.first_run_hidden = true;
        if remember {
            crate::persist::record(
                crate::persist::SettingsChange::ImportFirstRunPrompt(false),
                cx,
            );
        }
        cx.notify();
    }

    /// "Enable connection" on the navigator's selection: shows the reason and asks the core.
    pub fn enable_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(c) = self.selected(cx) else {
            return Self::notify_no_selection(window, cx);
        };
        if c.enabled {
            window.push_notification(
                Notification::info(format!("\"{}\" is already enabled.", c.name)),
                cx,
            );
            return;
        }
        let prompt = open_enable_prompt(self.hub.clone(), c.id, window, cx);
        let sub = cx.subscribe_in(
            &prompt,
            window,
            |this: &mut Self, _, event: &ConnectionEnabled, window, cx| {
                let name = this
                    .hub
                    .read(cx)
                    .connection(event.0)
                    .map(|c| c.name.clone())
                    .unwrap_or_default();
                window.push_notification(Notification::success(format!("Enabled \"{name}\".")), cx);
            },
        );
        self._subscriptions.push(sub);
    }

    /// "Lock secrets now": locks the vault and forgets the passwords asked this session
    /// (`CoreServices::lock_now`); the core's `SecretsLocked` event brings the notice.
    pub fn lock_secrets(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(core) = self.hub.read(cx).core().cloned() else {
            window.push_notification(
                Notification::info("NineSQL is still starting: there are no secrets to lock yet."),
                cx,
            );
            return;
        };
        drop(core.lock_now());
    }

    fn on_hub_event(
        &mut self,
        _: &Entity<ConnectionHub>,
        event: &HubEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            HubEvent::SecretsLocked(reason) => {
                window.push_notification(
                    Notification::info(format!(
                        "Secrets locked because {}: the vault needs its master password again, and passwords asked this session will be asked again.",
                        lock_reason_text(*reason)
                    ))
                    .title("Secrets locked"),
                    cx,
                );
                cx.notify();
            }
        }
    }

    /// Lists the problems found while loading connections (removed data, unknown keys that
    /// looked like secrets, a workspace or runtime that could not start, a desktop lock
    /// watcher that is not running).
    pub fn show_connection_problems(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let problems: Rc<Vec<SharedString>> = Rc::new(
            self.hub
                .read(cx)
                .problems()
                .iter()
                .map(|p| SharedString::from(p.clone()))
                .collect(),
        );
        window.open_dialog(cx, move |dialog, _window, _cx| {
            let body = v_flex()
                .gap_2()
                .text_sm()
                .when(problems.is_empty(), |el| {
                    el.child("No problems: every saved connection was loaded as it is.")
                })
                .children(
                    problems
                        .iter()
                        .enumerate()
                        .map(|(i, p)| div().id(("connection-problem", i)).child(p.clone())),
                );
            dialog.title("Connection problems").w(px(640.)).child(body)
        });
    }

    /// Focuses the active editor, or the workspace when the active pane is empty.
    pub fn focus_active(&self, window: &mut Window, cx: &mut App) {
        match self.layout.active_tab().and_then(|t| self.tabs.get(&t)) {
            Some(editor) => editor.focus_handle(cx).focus(window, cx),
            None => self.focus.focus(window, cx),
        }
    }

    fn after_change(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_active(window, cx);
        let title = match self.active_title(cx) {
            Some(tab) => format!("{tab} - NineSQL"),
            None => "NineSQL".to_owned(),
        };
        window.set_window_title(&title);
        cx.notify();
    }

    /// Opens the command palette.
    pub fn open_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        crate::palette::open(cx.entity().downgrade(), window, cx);
    }

    /// Lists the problems found in settings.toml in a dialog.
    pub fn show_settings_problems(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let state = cx
            .try_global::<SettingsState>()
            .cloned()
            .unwrap_or_default();
        let path: SharedString = state
            .path
            .as_ref()
            .map_or_else(
                || "no settings location".to_owned(),
                |p| p.display().to_string(),
            )
            .into();
        let problems: Rc<Vec<SharedString>> = Rc::new(
            state
                .diagnostics
                .iter()
                .map(|d| SharedString::from(d.message().to_owned()))
                .collect(),
        );
        window.open_dialog(cx, move |dialog, _window, cx| {
            let muted = cx.theme().muted_foreground;
            let body = v_flex()
                .gap_2()
                .text_sm()
                .child(div().text_xs().text_color(muted).child(path.clone()))
                .when(problems.is_empty(), |el| {
                    el.child("No problems: every setting and key binding was applied.")
                })
                .children(
                    problems
                        .iter()
                        .enumerate()
                        .map(|(i, p)| div().id(("settings-problem", i)).child(p.clone())),
                );
            dialog.title("Settings problems").w(px(640.)).child(body)
        });
    }

    /// Runs any registered action on this workspace (the command palette path). App-scope
    /// actions are forwarded to the application handlers. Returns false for an action that is
    /// not in the registry.
    pub fn perform(
        &mut self,
        action: &dyn Action,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(spec) = actions::spec_of(action) else {
            return false;
        };
        if spec.scope == Scope::App {
            let action = action.boxed_clone();
            cx.defer(move |cx| {
                crate::app::perform_app_action(action.as_ref(), cx);
            });
            return true;
        }
        let any = action.as_any();
        if any.is::<CommandPalette>() {
            self.open_palette(window, cx);
        } else if any.is::<NewEditorTab>() {
            self.new_tab(window, cx);
        } else if any.is::<CloseTab>() {
            self.close_active_tab(window, cx);
        } else if any.is::<NextTab>() {
            self.cycle_tab(true, window, cx);
        } else if any.is::<PreviousTab>() {
            self.cycle_tab(false, window, cx);
        } else if any.is::<SplitRight>() {
            self.split(SplitAxis::Horizontal, window, cx);
        } else if any.is::<SplitDown>() {
            self.split(SplitAxis::Vertical, window, cx);
        } else if any.is::<FocusNextPane>() {
            self.cycle_pane(true, window, cx);
        } else if any.is::<FocusPreviousPane>() {
            self.cycle_pane(false, window, cx);
        } else if any.is::<ToggleNavigator>() {
            self.toggle_navigator(window, cx);
        } else if any.is::<FocusNavigator>() {
            self.focus_navigator(window, cx);
        } else if any.is::<ShowSettingsProblems>() {
            self.show_settings_problems(window, cx);
        } else if any.is::<NewConnection>() {
            self.new_connection(window, cx);
        } else if any.is::<EditConnection>() {
            self.edit_connection(window, cx);
        } else if any.is::<DuplicateConnection>() {
            self.duplicate_connection(window, cx);
        } else if any.is::<DeleteConnection>() {
            self.delete_connection(window, cx);
        } else if any.is::<ConnectConnection>() {
            self.connect_selected(Purpose::Open, window, cx);
        } else if any.is::<TestConnection>() {
            self.connect_selected(Purpose::Test, window, cx);
        } else if any.is::<DisconnectConnection>() {
            self.disconnect_selected(window, cx);
        } else if any.is::<ShowConnectionProblems>() {
            self.show_connection_problems(window, cx);
        } else if any.is::<LockSecrets>() {
            self.lock_secrets(window, cx);
        } else if any.is::<ImportFromDbeaver>() {
            self.import_from_dbeaver(window, cx);
        } else if any.is::<ImportFromDbeaverFolder>() {
            self.import_from_dbeaver_folder(window, cx);
        } else if any.is::<EnableConnection>() {
            self.enable_selected(window, cx);
        } else if any.is::<CloseWindow>() {
            window.remove_window();
        } else {
            return false;
        }
        true
    }

    fn render_pane(&self, pane: PaneId, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(model) = self.layout.pane(pane) else {
            return div().into_any_element();
        };
        let is_active = self.layout.active_pane_id() == pane;
        let multi_pane = self.layout.pane_count() > 1;
        let this = cx.entity().downgrade();
        let theme = cx.theme();
        let tabs: Vec<Tab> = model
            .tabs()
            .iter()
            .map(|tab| {
                let title = self
                    .tabs
                    .get(tab)
                    .map(|e| e.read(cx).title().clone())
                    .unwrap_or_default();
                let tab_id = *tab;
                let this = this.clone();
                Tab::new().label(title.clone()).suffix(
                    Button::new(("close-tab", tab.get()))
                        .ghost()
                        .xsmall()
                        .icon(IconName::Close)
                        .tooltip(format!("Close {title}"))
                        .on_click(move |_, window, cx| {
                            let _ = this.update(cx, |ws, cx| ws.close_tab(tab_id, window, cx));
                        }),
                )
            })
            .collect();
        let tab_ids: Vec<TabId> = model.tabs().to_vec();
        let new_tab_hint = cx
            .try_global::<EffectiveKeymap>()
            .and_then(|k| k.display_for("ninesql::NewEditorTab"));
        let new_tab_tooltip = match &new_tab_hint {
            Some(keys) => format!("New SQL editor ({keys})"),
            None => "New SQL editor".to_owned(),
        };
        let mut bar = TabBar::new(("tab-bar", pane.get()))
            .children(tabs)
            .suffix({
                let this = this.clone();
                Button::new(("new-tab", pane.get()))
                    .ghost()
                    .xsmall()
                    .icon(IconName::Plus)
                    .tooltip(new_tab_tooltip)
                    .on_click(move |_, window, cx| {
                        let _ = this.update(cx, |ws, cx| {
                            ws.layout.activate_pane(pane);
                            ws.new_tab(window, cx);
                        });
                    })
            })
            .on_click({
                let this = this.clone();
                move |ix, window, cx| {
                    if let Some(tab) = tab_ids.get(*ix).copied() {
                        let _ = this.update(cx, |ws, cx| ws.activate_tab(tab, window, cx));
                    }
                }
            });
        if let Some(ix) = model.active_index() {
            bar = bar.selected_index(ix);
        }
        let content = match model.active_tab().and_then(|t| self.tabs.get(&t)) {
            Some(editor) => div()
                .flex_1()
                .min_h_0()
                .child(editor.clone())
                .into_any_element(),
            None => {
                let hint = match new_tab_hint {
                    Some(keys) => format!("No open editor. Press {keys} to open one."),
                    None => "No open editor.".to_owned(),
                };
                div()
                    .id(("empty-pane", pane.get()))
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(hint)
                    .into_any_element()
            }
        };
        let accent = if is_active && multi_pane {
            theme.ring
        } else {
            theme.border
        };
        let _ = window;
        v_flex()
            .id(("pane", pane.get()))
            .size_full()
            .border_t_2()
            .border_color(accent)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |ws, _, _, cx| ws.activate_pane(pane, cx)),
            )
            .child(bar)
            .child(content)
            .into_any_element()
    }

    fn render_node(&self, node: &Node, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        match node {
            Node::Pane(pane) => self.render_pane(*pane, window, cx),
            Node::Split { axis, children } => {
                // Keyed by the first pane, which stays in place when siblings are added.
                let mut first = Vec::new();
                collect_first(node, &mut first);
                let key = first.first().map_or(0, |p| p.get());
                let group = match axis {
                    SplitAxis::Horizontal => h_resizable(("split-h", key)),
                    SplitAxis::Vertical => v_resizable(("split-v", key)),
                };
                let panels: Vec<_> = children
                    .iter()
                    .map(|child| resizable_panel().child(self.render_node(child, window, cx)))
                    .collect();
                group.children(panels).into_any_element()
            }
        }
    }

    fn render_status_bar(&self, window: &mut Window, cx: &mut Context<Self>) -> StatusBar {
        let active = self.active_connection.and_then(|id| {
            let hub = self.hub.read(cx);
            let c = hub.connection(id)?;
            Some((
                c.name.clone(),
                ConnectionBadges::of(c, hub.types(), hub.sessions(id)),
                hub.is_open(id),
            ))
        });
        let connection: AnyElement = match active {
            Some((name, b, open)) => h_flex()
                .id("status-connection")
                .gap_2()
                .child(if open {
                    name
                } else {
                    format!("{name} (disconnected)")
                })
                .when(b.pending_reconnect, |el| {
                    el.child(
                        div()
                            .id("status-pending-reconnect")
                            .text_xs()
                            .child("edited: reconnect to apply"),
                    )
                })
                // ADR-0007 §4: the type name is always visible in the status bar.
                .child(badges("status", &b, true, cx))
                .into_any_element(),
            None => div()
                .id("status-connection")
                .child(
                    Services::bridge(cx)
                        .active_connection_label()
                        .unwrap_or_else(|| "No connection".into()),
                )
                .into_any_element(),
        };
        let connection_problems = self.hub.read(cx).problems().len();
        let (starting, locked) = {
            let hub = self.hub.read(cx);
            (hub.is_starting(), hub.secrets_locked())
        };
        let problems = cx
            .try_global::<SettingsState>()
            .map_or(0, |s| s.diagnostics.len());
        let mode = if appearance::current_mode(cx).is_dark() {
            "Dark"
        } else {
            "Light"
        };
        let scale = format!("Scale {:.0}%", window.scale_factor() * 100.0);
        let position = format!(
            "Pane {}/{}",
            self.layout
                .pane_order()
                .iter()
                .position(|p| *p == self.layout.active_pane_id())
                .map_or(1, |i| i + 1),
            self.layout.pane_count()
        );
        let mut bar = StatusBar::new().left(connection);
        if connection_problems > 0 {
            let this = cx.entity().downgrade();
            bar = bar.left(
                Button::new("status-connection-problems")
                    .ghost()
                    .xsmall()
                    .label(if connection_problems == 1 {
                        "1 connection problem".to_owned()
                    } else {
                        format!("{connection_problems} connection problems")
                    })
                    .text_color(appearance::ensure_contrast(
                        cx.theme().warning,
                        &[*cx.theme().tokens.status_bar],
                        appearance::MIN_TEXT_CONTRAST,
                    ))
                    .on_click(move |_, window, cx| {
                        let _ = this.update(cx, |ws, cx| ws.show_connection_problems(window, cx));
                    }),
            );
        }
        if starting {
            bar = bar.left(div().id("status-starting").text_xs().child("Starting…"));
        }
        if let Some(reason) = locked {
            bar = bar.left(
                div()
                    .id("status-secrets-locked")
                    .text_xs()
                    .child(format!("Secrets locked ({})", lock_reason_text(reason))),
            );
        }
        if problems > 0 {
            let label = if problems == 1 {
                "1 settings problem".to_owned()
            } else {
                format!("{problems} settings problems")
            };
            let this = cx.entity().downgrade();
            bar = bar.left(
                Button::new("status-settings-problems")
                    .ghost()
                    .xsmall()
                    .label(label)
                    .text_color(appearance::ensure_contrast(
                        cx.theme().warning,
                        &[*cx.theme().tokens.status_bar],
                        appearance::MIN_TEXT_CONTRAST,
                    ))
                    .on_click(move |_, window, cx| {
                        let _ = this.update(cx, |ws, cx| ws.show_settings_problems(window, cx));
                    }),
            );
        }
        bar.right(div().id("status-pane").child(position))
            .right(div().id("status-theme").child(mode))
            .right(div().id("status-scale").child(scale))
    }
}

impl Workspace {
    /// The first-run "Import from DBeaver" screen, above the editor area.
    fn render_first_run(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let found = self.first_run_offer(cx)?;
        let keys = cx
            .try_global::<EffectiveKeymap>()
            .and_then(|k| k.display_for("ninesql::ImportFromDbeaver"));
        let this = cx.entity().downgrade();
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let where_ = found
            .iter()
            .map(|w| {
                format!(
                    "{} ({}, {} project(s))",
                    w.path.display(),
                    crate::import::model::install_label(w.kind),
                    w.projects
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        let button = |id: &'static str, label: &'static str| Button::new(id).small().label(label);
        Some(
            v_flex()
                .id("first-run-import")
                .m_2()
                .p_3()
                .gap_2()
                .border_1()
                .rounded_md()
                .border_color(theme.border)
                .bg(theme.secondary)
                .text_color(theme.secondary_foreground)
                .text_sm()
                .child(
                    div()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child("Import your DBeaver connections?"),
                )
                .child(format!("A DBeaver workspace was found: {where_}."))
                .child(div().text_xs().text_color(muted).child(
                    "You see what is imported before anything is saved. Saved passwords move to the keyring or the vault; DBeaver's files are only read, never changed.",
                ))
                .child(
                    h_flex()
                        .gap_2()
                        .child({
                            let this = this.clone();
                            button("first-run-import-start", "Import from DBeaver")
                                .primary()
                                .when_some(keys, |b, k| b.tooltip(format!("Import from DBeaver ({k})")))
                                .on_click(move |_, window, cx| {
                                    let _ = this.update(cx, |ws, cx| ws.import_from_dbeaver(window, cx));
                                })
                        })
                        .child({
                            let this = this.clone();
                            button("first-run-import-folder", "Choose folder…").on_click(move |_, window, cx| {
                                let _ = this.update(cx, |ws, cx| ws.import_from_dbeaver_folder(window, cx));
                            })
                        })
                        .child({
                            let this = this.clone();
                            button("first-run-import-later", "Not now").ghost().on_click(move |_, _, cx| {
                                let _ = this.update(cx, |ws, cx| ws.dismiss_first_run(false, cx));
                            })
                        })
                        .child({
                            let this = this.clone();
                            button("first-run-import-never", "Don't show again").ghost().on_click(move |_, _, cx| {
                                let _ = this.update(cx, |ws, cx| ws.dismiss_first_run(true, cx));
                            })
                        }),
                )
                .into_any_element(),
        )
    }

    /// The color framing the editor area (ADR-0007 §4): the type color of the active
    /// connection when production rules apply.
    #[must_use]
    pub fn frame_color(&self, cx: &App) -> Option<Hsla> {
        let id = self.active_connection?;
        let hub = self.hub.read(cx);
        let c = hub.connection(id)?;
        let b = ConnectionBadges::of(c, hub.types(), hub.sessions(id));
        b.production.then_some(b.color)
    }

    fn frame_editor_area(&self, area: AnyElement, cx: &App) -> AnyElement {
        match self.frame_color(cx) {
            Some(color) => div()
                .id("production-frame")
                .size_full()
                .border_2()
                .border_color(color)
                .child(area)
                .into_any_element(),
            None => area,
        }
    }
}

fn collect_first(node: &Node, out: &mut Vec<PaneId>) {
    match node {
        Node::Pane(p) => out.push(*p),
        Node::Split { children, .. } => {
            if let Some(c) = children.first() {
                collect_first(c, out);
            }
        }
    }
}

impl Focusable for Workspace {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

macro_rules! on {
    ($el:expr, $cx:expr, $($action:ty),+ $(,)?) => {{
        let mut el = $el;
        $(
            el = el.on_action($cx.listener(|this, action: &$action, window, cx| {
                this.perform(action, window, cx);
            }));
        )+
        el
    }};
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let client_decorated = matches!(window.window_decorations(), Decorations::Client { .. });
        let editor_area = self.render_node(&self.layout.root().clone(), window, cx);
        let status_bar = self.render_status_bar(window, cx);
        let theme = cx.theme();
        let root = v_flex()
            .id("workspace")
            .key_context(WORKSPACE_CONTEXT)
            .track_focus(&self.focus)
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .font_family(theme.font_family.clone())
            .text_size(theme.font_size);
        let root = on!(
            root,
            cx,
            CommandPalette,
            NewEditorTab,
            CloseTab,
            NextTab,
            PreviousTab,
            SplitRight,
            SplitDown,
            FocusNextPane,
            FocusPreviousPane,
            ToggleNavigator,
            FocusNavigator,
            ShowSettingsProblems,
            CloseWindow,
            NewConnection,
            EditConnection,
            DuplicateConnection,
            DeleteConnection,
            ConnectConnection,
            DisconnectConnection,
            TestConnection,
            ShowConnectionProblems,
            LockSecrets,
            ImportFromDbeaver,
            ImportFromDbeaverFolder,
            EnableConnection,
        );
        let content = root
            .when(client_decorated, |el| {
                el.child(
                    TitleBar::new().child(
                        h_flex().gap_2().text_sm().child("NineSQL").children(
                            self.active_title(cx)
                                .map(|t| div().text_color(cx.theme().muted_foreground).child(t)),
                        ),
                    ),
                )
            })
            .child(
                div().flex_1().min_h_0().child(
                    h_resizable("workspace-docks")
                        .child(
                            resizable_panel()
                                .size(NAVIGATOR_WIDTH)
                                .size_range(px(160.)..px(560.))
                                .visible(self.navigator_visible)
                                .child(self.navigator.clone()),
                        )
                        .child(
                            resizable_panel().child(
                                v_flex()
                                    .size_full()
                                    .children(self.render_first_run(cx))
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_h_0()
                                            .child(self.frame_editor_area(editor_area, cx)),
                                    ),
                            ),
                        ),
                ),
            )
            .child(status_bar);
        if client_decorated {
            window_border().child(content).into_any_element()
        } else {
            content.into_any_element()
        }
    }
}
