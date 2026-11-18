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
    self, ChooseEditorConnection, CloseTab, CloseWindow, CommandPalette, CommitTransaction,
    ConnectConnection, DeleteConnection, DisconnectConnection, DuplicateConnection, EditConnection,
    EnableConnection, FocusNavigator, FocusNextPane, FocusPreviousPane, ImportFromDbeaver,
    ImportFromDbeaverFolder, LockSecrets, NewConnection, NewEditorTab, NextTab, PreviousTab,
    RollbackTransaction, Scope, ShowConnectionProblems, ShowSettingsProblems, SplitDown,
    SplitRight, TestConnection, ToggleNavigator, WORKSPACE_CONTEXT,
};
use crate::actions::{
    ClearTableFilter, DeleteRows, DiscardChanges, DuplicateRow, EditCell, ExportToDbeaver,
    FilterTableBySelectedValue, FilterTableRows, InsertRow, OpenReferencedRows,
    OpenReferencingRows, OpenTableData, PreviewChanges, RefreshTableData, RevertRowChanges,
    SaveChanges, SetCellNull, SortTableAscending, SortTableDescending,
};
use crate::actions::{
    ConfigureNavigatorFilters, CopyObjectDdl, FindObject, GenerateDeleteSql, GenerateInsertSql,
    GenerateSelectSql, GenerateUpdateSql, LoadMoreObjects, OpenObjectEditor, RefreshNavigatorNode,
    ShowObjectDdl,
};
use crate::actions::{ExportResults, ExportTable, FormatSql, ImportCsv, InsertSnippet};
use crate::appearance;
use crate::browse::generate::{KeySource, Template, TemplateInput, names_survive_editor, template};
use crate::browse::object_editor::{ObjectEditor, ObjectEditorEvent};
use crate::browse::search::{FindChoice, SearchScope, open_find_object};
use crate::browse::{is_table_like, kind_singular};
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
use crate::navigator::{NavigatorEvent, Selection};
use crate::services::Services;
use crate::settings::SettingsState;
use gpui_kit::component::notification::Notification;
use ninesql_core::ConnectionId;
use ninesql_core::NodeKey;
use ninesql_core::driver_api::ObjectKind;
use ninesql_core::driver_api::metadata::{ObjectInfo, ObjectPath};

/// What runs once sessions were closed as the user chose (see `Workspace::close_sessions_then`).
type AfterClose = Rc<dyn Fn(&mut Workspace, &mut Window, &mut Context<Workspace>)>;

/// Width of the navigator dock when the window opens.
const NAVIGATOR_WIDTH: Pixels = px(260.);

/// The workspace view of one window.
pub struct Workspace {
    focus: FocusHandle,
    layout: EditorLayout,
    tabs: HashMap<TabId, Entity<SqlEditorTab>>,
    objects: HashMap<TabId, (Entity<ObjectEditor>, Subscription)>,
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
        let navigator = cx.new(Navigator::new);
        let subscriptions = vec![
            cx.observe(&hub, |_, _, cx| cx.notify()),
            cx.subscribe_in(&flow, window, Self::on_flow_finished),
            cx.subscribe_in(&hub, window, Self::on_hub_event),
            cx.subscribe_in(&navigator, window, Self::on_navigator_event),
        ];
        let mut this = Self {
            focus: cx.focus_handle(),
            layout: EditorLayout::new(),
            tabs: HashMap::new(),
            objects: HashMap::new(),
            navigator,
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
            .filter_map(|t| self.title_of(t, cx))
            .collect()
    }

    fn title_of(&self, tab: TabId, cx: &App) -> Option<SharedString> {
        if let Some(e) = self.tabs.get(&tab) {
            return Some(e.read(cx).title().clone());
        }
        self.objects
            .get(&tab)
            .map(|(o, _)| o.read(cx).title().clone())
    }

    /// The object editor of a tab.
    #[must_use]
    pub fn object_editor(&self, tab: TabId) -> Option<&Entity<ObjectEditor>> {
        self.objects.get(&tab).map(|(o, _)| o)
    }

    /// The object editor of the active pane, if its active tab is one.
    #[must_use]
    pub fn active_object_editor(&self) -> Option<&Entity<ObjectEditor>> {
        self.layout
            .active_tab()
            .and_then(|t| self.objects.get(&t))
            .map(|(o, _)| o)
    }

    /// Title of the active tab.
    #[must_use]
    pub fn active_title(&self, cx: &App) -> Option<SharedString> {
        let tab = self.layout.active_tab()?;
        self.title_of(tab, cx)
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
        // A new editor runs on the active editor's session, else on the last connection
        // opened in this window.
        let session = self
            .active_editor()
            .and_then(|t| t.read(cx).session().cloned())
            .or_else(|| {
                let id = self.active_connection?;
                self.hub.read(cx).sessions(id).last().cloned()
            });
        let hub = self.hub.clone();
        let editor = cx.new(|cx| SqlEditorTab::new(title, hub, session, window, cx));
        self.tabs.insert(id, editor);
        id
    }

    /// The editor tab of the active pane.
    #[must_use]
    pub fn active_editor(&self) -> Option<&Entity<SqlEditorTab>> {
        self.layout.active_tab().and_then(|t| self.tabs.get(&t))
    }

    /// Opens an empty SQL editor in the active pane.
    pub fn new_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) -> TabId {
        let id = self.create_tab(window, cx);
        self.layout.open_tab(id);
        self.after_change(window, cx);
        id
    }

    /// Closes a tab. When its session has an open transaction that the preferences ask about
    /// (always for production, ADR-0007 §5) and no other editor of this window uses that
    /// session, asks first: commit, roll back, or keep the tab.
    pub fn close_tab(&mut self, tab: TabId, window: &mut Window, cx: &mut Context<Self>) {
        if self.objects.contains_key(&tab) {
            return self.close_tab_now(tab, window, cx);
        }
        let Some(editor) = self.tabs.get(&tab).cloned() else {
            return;
        };
        let session = editor.read(cx).session().cloned();
        let shared = session.as_ref().is_some_and(|s| {
            self.tabs
                .iter()
                .any(|(t, e)| *t != tab && e.read(cx).session().is_some_and(|o| o.id() == s.id()))
        });
        if !shared && editor.read(cx).close_needs_confirmation(cx) {
            let this = cx.entity().downgrade();
            let name = session
                .as_ref()
                .map(|s| s.info().connection_name.clone())
                .unwrap_or_default();
            crate::editor_tab::open_close_confirmation(
                name,
                Rc::new(move |commit, window, cx| {
                    let _ = this.update(cx, |ws, cx| {
                        ws.end_transaction_and_close_tab(tab, commit, window, cx);
                    });
                }),
                window,
                cx,
            );
            return;
        }
        self.close_tab_now(tab, window, cx);
    }

    /// The close confirmation's choice: commits (`commit = true`) or rolls back the tab's
    /// transaction, and closes the tab only once that succeeded. On an error (a run going on, a
    /// failed commit) the tab stays open with the error shown: nothing is lost silently
    /// (ADR-0007 §5).
    pub fn end_transaction_and_close_tab(
        &mut self,
        tab: TabId,
        commit: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(editor) = self.tabs.get(&tab).cloned() else {
            return;
        };
        let ended = editor.update(cx, |e, cx| e.end_transaction(commit, cx));
        cx.spawn_in(window, async move |this, cx| {
            let result = ended.await;
            let _ = this.update_in(cx, |ws, window, cx| match result {
                Ok(()) => ws.close_tab_now(tab, window, cx),
                Err(e) => {
                    window.push_notification(Notification::error(e).title("Tab not closed"), cx)
                }
            });
        })
        .detach();
    }

    /// Closes a tab without asking.
    pub fn close_tab_now(&mut self, tab: TabId, window: &mut Window, cx: &mut Context<Self>) {
        if self.layout.close_tab(tab) {
            self.tabs.remove(&tab);
            // Dropping an object editor drops its loads (cancelled in the core).
            self.objects.remove(&tab);
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

    /// Deletes a connection (the confirmed path of "Delete connection"). Its open sessions
    /// close first: one with an open transaction that the preferences ask about (always for
    /// production, ADR-0007 §5) asks how to end it, as "Disconnect" does; an error there keeps
    /// the connection.
    pub fn delete_connection_now(
        &mut self,
        id: ConnectionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let asking = self.sessions_asking_on_close(id, cx);
        if !asking.is_empty() {
            return self.end_transactions_then(
                asking,
                "Not deleted",
                Rc::new(move |ws, window, cx| ws.remove_connection_now(id, window, cx)),
                window,
                cx,
            );
        }
        self.remove_connection_now(id, window, cx);
    }

    /// Removes a connection without asking (its sessions are closed by the hub).
    fn remove_connection_now(
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
        let asking = self.sessions_asking_on_close(id, cx);
        if !asking.is_empty() {
            return self.end_transactions_then(
                asking,
                "Not disconnected",
                Rc::new(move |ws, _, cx| ws.disconnect_now(id, cx)),
                window,
                cx,
            );
        }
        self.disconnect_now(id, cx);
    }

    /// The open sessions of `id` whose close must ask first: an open transaction the
    /// preferences ask about (always for production, ADR-0007 §5).
    fn sessions_asking_on_close(
        &self,
        id: ConnectionId,
        cx: &App,
    ) -> Vec<ninesql_core::SessionHandle> {
        let hub = self.hub.read(cx);
        let prefs = hub.preferences();
        hub.sessions(id)
            .iter()
            .filter(|s| s.close_needs_confirmation(&prefs.confirmations))
            .cloned()
            .collect()
    }

    /// Asks how to end the open transactions of `asking` (ADR-0007 §5: never ended silently),
    /// then [`Self::close_sessions_then`] with the choice; Cancel does nothing.
    fn end_transactions_then(
        &self,
        asking: Vec<ninesql_core::SessionHandle>,
        error_title: &'static str,
        then: AfterClose,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(first) = asking.first() else {
            return;
        };
        let name = first.info().connection_name.clone();
        let this = cx.entity().downgrade();
        crate::editor_tab::open_close_confirmation(
            name,
            Rc::new(move |commit, window, cx| {
                let (asking, then) = (asking.clone(), then.clone());
                let _ = this.update(cx, |ws, cx| {
                    ws.close_sessions_then(asking, commit, error_title, then, window, cx);
                });
            }),
            window,
            cx,
        );
    }

    /// Ends the open transaction of each of `sessions` (commit or roll back) and closes them,
    /// then runs `then`. The first failure stops there, with a notification titled
    /// `error_title` (a failed commit leaves its session open).
    fn close_sessions_then(
        &self,
        sessions: Vec<ninesql_core::SessionHandle>,
        commit: bool,
        error_title: &'static str,
        then: AfterClose,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let end = if commit {
            ninesql_core::TransactionEnd::Commit
        } else {
            ninesql_core::TransactionEnd::Rollback
        };
        cx.spawn_in(window, async move |this, cx| {
            for s in &sessions {
                if let Err(e) = s.close_with(end).await {
                    let text = crate::editor_tab::run::core_error_text(&e);
                    let _ = cx.update(|window, cx| {
                        window.push_notification(Notification::error(text).title(error_title), cx);
                    });
                    return;
                }
            }
            let _ = this.update_in(cx, |ws, window, cx| then(ws, window, cx));
        })
        .detach();
    }

    /// The close confirmation's choice for "Delete connection": ends the open transactions of
    /// its sessions (commit or roll back) and closes them, then deletes the connection; a
    /// failure keeps the connection.
    pub fn delete_connection_ending_transactions(
        &mut self,
        id: ConnectionId,
        commit: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let asking = self.sessions_asking_on_close(id, cx);
        self.close_sessions_then(
            asking,
            commit,
            "Not deleted",
            Rc::new(move |ws, window, cx| ws.remove_connection_now(id, window, cx)),
            window,
            cx,
        );
    }

    /// Closes the sessions of a connection without asking.
    pub fn disconnect_now(&mut self, id: ConnectionId, cx: &mut Context<Self>) {
        self.hub.update(cx, |hub, cx| hub.disconnect(id, cx));
        if self.active_connection == Some(id) {
            self.active_connection = None;
        }
        cx.notify();
    }

    /// "Choose editor connection": the open sessions, for the active editor.
    pub fn choose_editor_connection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.active_editor().cloned() else {
            return window.push_notification(Notification::info("Open an SQL editor first."), cx);
        };
        if editor.read(cx).is_running() {
            return window.push_notification(
                Notification::info(
                    "A run is going on in this editor: cancel it before choosing another connection.",
                ),
                cx,
            );
        }
        let sessions = self.hub.read(cx).open_sessions();
        let current = editor
            .read(cx)
            .session()
            .map(ninesql_core::SessionHandle::id);
        crate::editor_tab::open_connection_picker(editor, sessions, current, window, cx);
    }

    /// Commits or rolls back the active editor's transaction.
    pub fn end_transaction(&mut self, commit: bool, cx: &mut Context<Self>) {
        if let Some(editor) = self.active_editor().cloned() {
            editor.update(cx, |e, cx| e.end_transaction(commit, cx).detach());
        }
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
                // The active editor without a connection takes the session just opened; never
                // one with a run going on (its run stays on the session it started on).
                if let Some(editor) = self.active_editor().cloned()
                    && !editor.read(cx).is_running()
                    && editor.read(cx).session().is_none()
                    && let Some(session) = self.hub.read(cx).sessions(*connection).last().cloned()
                {
                    editor.update(cx, |e, cx| e.set_session(Some(session), cx));
                }
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
            // The editor tabs follow their own session's transaction state.
            HubEvent::TransactionChanged { .. } => cx.notify(),
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
        let tab = self.layout.active_tab();
        if let Some(editor) = tab.and_then(|t| self.tabs.get(&t)) {
            editor.focus_handle(cx).focus(window, cx);
        } else if let Some((object, _)) = tab.and_then(|t| self.objects.get(&t)) {
            object.focus_handle(cx).focus(window, cx);
        } else {
            self.focus.focus(window, cx);
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
        if matches!(spec.scope, Scope::Editor | Scope::Grid) {
            // The palette path of the editor's and the grid's actions: focus the active editor
            // (or its results) and dispatch there, as the key binding would.
            let Some(editor) = self.active_editor().cloned() else {
                return true;
            };
            let focus = if spec.scope == Scope::Grid {
                editor.read(cx).results().focus_handle(cx)
            } else {
                editor.focus_handle(cx)
            };
            focus.focus(window, cx);
            window.dispatch_action(action.boxed_clone(), cx);
            return true;
        }
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
        } else if any.is::<ChooseEditorConnection>() {
            self.choose_editor_connection(window, cx);
        } else if any.is::<CommitTransaction>() {
            self.end_transaction(true, cx);
        } else if any.is::<RollbackTransaction>() {
            self.end_transaction(false, cx);
        } else if any.is::<RefreshNavigatorNode>() {
            self.refresh_navigator_node(window, cx);
        } else if any.is::<LoadMoreObjects>() {
            self.load_more_objects(window, cx);
        } else if any.is::<ConfigureNavigatorFilters>() {
            self.configure_navigator_filters(window, cx);
        } else if any.is::<FindObject>() {
            self.find_object(window, cx);
        } else if any.is::<OpenObjectEditor>() {
            self.open_selected_object(window, cx);
        } else if any.is::<GenerateSelectSql>() {
            self.generate_selected(Template::Select, window, cx);
        } else if any.is::<GenerateInsertSql>() {
            self.generate_selected(Template::Insert, window, cx);
        } else if any.is::<GenerateUpdateSql>() {
            self.generate_selected(Template::Update, window, cx);
        } else if any.is::<GenerateDeleteSql>() {
            self.generate_selected(Template::Delete, window, cx);
        } else if any.is::<ShowObjectDdl>() {
            self.show_selected_ddl(window, cx);
        } else if any.is::<CopyObjectDdl>() {
            self.copy_object_ddl(window, cx);
        } else if any.is::<FormatSql>() {
            self.format_sql(window, cx);
        } else if any.is::<InsertSnippet>() {
            self.insert_snippet(window, cx);
        } else if any.is::<ExportResults>() {
            self.export_results(window, cx);
        } else if any.is::<ExportTable>() {
            self.export_table(window, cx);
        } else if any.is::<ImportCsv>() {
            self.import_csv(window, cx);
        } else if any.is::<OpenTableData>() {
            self.open_table_data(window, cx);
        } else if any.is::<ExportToDbeaver>() {
            self.export_to_dbeaver(window, cx);
        } else if any.is::<OpenReferencedRows>() {
            self.open_foreign_key_rows(false, window, cx);
        } else if any.is::<OpenReferencingRows>() {
            self.open_foreign_key_rows(true, window, cx);
        } else if self.table_action(any, window, cx) {
            // Handled by the active editor's table.
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
                let title = self.title_of(*tab, cx).unwrap_or_default();
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
        let object = model
            .active_tab()
            .and_then(|t| self.objects.get(&t))
            .map(|(o, _)| o.clone());
        let content = match model.active_tab().and_then(|t| self.tabs.get(&t)) {
            None if object.is_some() => {
                div().flex_1().min_h_0().children(object).into_any_element()
            }
            Some(editor) => div()
                .flex_1()
                .min_h_0()
                // With several panes, each production editor gets its own frame, whichever
                // pane is active (ADR-0007 §4).
                .when_some(
                    multi_pane
                        .then(|| self.pane_frame_color(pane, cx))
                        .flatten(),
                    |el, color| el.border_2().border_color(color),
                )
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

    /// The color framing the editor area (ADR-0007 §4): the type color of the active editor's
    /// session when production rules apply; without an editor session, of the last
    /// connection opened in this window.
    #[must_use]
    pub fn frame_color(&self, cx: &App) -> Option<Hsla> {
        if let Some(editor) = self.active_editor()
            && editor.read(cx).session().is_some()
        {
            return editor.read(cx).frame_color(cx);
        }
        let id = self.active_connection?;
        let hub = self.hub.read(cx);
        let c = hub.connection(id)?;
        let b = ConnectionBadges::of(c, hub.types(), hub.sessions(id));
        b.production.then_some(b.color)
    }

    /// The production frame color of a pane: its active editor's, when production rules apply
    /// to that editor's session.
    #[must_use]
    pub fn pane_frame_color(&self, pane: PaneId, cx: &App) -> Option<Hsla> {
        let tab = self.layout.pane(pane)?.active_tab()?;
        self.tabs.get(&tab)?.read(cx).frame_color(cx)
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

/// Refusal of a write template on a read-only session (I8).
pub const READ_ONLY_TEMPLATES: &str =
    "This session is read-only: only the SELECT template is offered.";
/// Refusal of a template whose names the SQL editor would change (P4-B review M2).
pub const UNSAFE_NAMES: &str = "This object or one of its columns has a name with control or line-separator characters that the SQL editor would change, so a template could target another object. No template was opened.";
/// Show DDL with text the SQL editor would change (P4-B review M2).
pub const UNSAFE_DDL: &str = "This DDL contains control or line-separator characters that the SQL editor would change: it is shown read-only in the object editor instead.";

/// Metadata browsing (P4-B): the navigator's actions, "Find object", the object editor and
/// "Generate SQL".
impl Workspace {
    fn on_navigator_event(
        &mut self,
        _: &Entity<Navigator>,
        event: &NavigatorEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            NavigatorEvent::Connect(id) => {
                if !self.flow.read(cx).is_busy() {
                    self.connect(*id, Purpose::Open, window, cx);
                }
            }
            NavigatorEvent::OpenObject { connection, path } => {
                self.open_object_editor(*connection, path.clone(), None, window, cx);
            }
            NavigatorEvent::RevealFailed(message) => {
                window.push_notification(Notification::info(message.clone()), cx);
            }
        }
    }

    fn on_object_editor_event(
        &mut self,
        editor: &Entity<ObjectEditor>,
        event: &ObjectEditorEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ObjectEditorEvent::Generate(which) = event;
        let (connection, path) = {
            let e = editor.read(cx);
            (e.connection(), e.path().clone())
        };
        self.generate(connection, path, *which, window, cx);
    }

    fn notify(message: impl Into<SharedString>, window: &mut Window, cx: &mut App) {
        window.push_notification(Notification::info(message.into()), cx);
    }

    /// The navigator's selection.
    fn navigator_selection(&self, cx: &App) -> Option<Selection> {
        self.navigator.read(cx).selection()
    }

    /// The object the metadata actions apply to: the navigator's selected object, else the
    /// active object editor's.
    fn target_object(&self, cx: &App) -> Option<(ConnectionId, ObjectPath, Option<ObjectInfo>)> {
        if let Some(Selection::Object(c, info)) = self.navigator_selection(cx) {
            return Some((c, info.path.clone(), Some(info)));
        }
        let editor = self.active_object_editor()?.read(cx);
        Some((
            editor.connection(),
            editor.path().clone(),
            editor.info().cloned(),
        ))
    }

    /// "Refresh navigator node".
    pub fn refresh_navigator_node(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(editor) = self.active_object_editor().cloned()
            && !self.navigator.focus_handle(cx).contains_focused(window, cx)
        {
            editor.update(cx, |e, cx| e.refresh(window, cx));
            return;
        }
        let done = self.navigator.update(cx, |n, cx| n.refresh_selected(cx));
        if !done {
            Self::notify(
                "Select an open connection, a group or an object in the navigator first.",
                window,
                cx,
            );
        }
    }

    /// "Load more objects".
    pub fn load_more_objects(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let done = self.navigator.update(cx, |n, cx| n.load_more_selected(cx));
        if !done {
            Self::notify(
                "The selected group has no more objects to load.",
                window,
                cx,
            );
        }
    }

    /// The connection the metadata dialogs apply to: the navigator's selection, else the
    /// active editor's.
    fn metadata_connection(&self, cx: &App) -> Option<ConnectionId> {
        self.navigator_selection(cx)
            .map(|s| s.connection())
            .or_else(|| self.active_editor().and_then(|e| e.read(cx).connection()))
            .or_else(|| self.active_object_editor().map(|e| e.read(cx).connection()))
    }

    /// "Configure navigator filters": the kinds come from the open session's declared
    /// capabilities, else from the driver's (I8).
    pub fn configure_navigator_filters(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.metadata_connection(cx) else {
            return Self::notify_no_selection(window, cx);
        };
        let hub = self.hub.read(cx);
        let Some(service) = hub.metadata() else {
            return Self::notify(
                "Metadata is not available: the driver runtime did not start.",
                window,
                cx,
            );
        };
        let Some(c) = hub.connection(id) else {
            return;
        };
        let declared = match hub.sessions(id).first() {
            Some(s) => s.info().capabilities.metadata.kinds.clone(),
            None => hub
                .registry()
                .capabilities(&c.engine)
                .map(|caps| caps.metadata.kinds)
                .unwrap_or_default(),
        };
        let kinds: Vec<ObjectKind> = ninesql_core::metadata::model::KIND_NAMES
            .iter()
            .map(|(k, _)| *k)
            .filter(|k| declared.contains(k))
            .collect();
        let name = c.name.clone();
        crate::browse::filters::open_filter_dialog(service, id, name, kinds, window, cx);
    }

    /// "Find object" over the open connections.
    pub fn find_object(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let hub = self.hub.read(cx);
        let Some(service) = hub.metadata() else {
            return Self::notify(
                "Metadata is not available: the driver runtime did not start.",
                window,
                cx,
            );
        };
        let mut scopes: Vec<SearchScope> = Vec::new();
        for s in hub.open_sessions() {
            let id = s.info().connection;
            if !scopes.iter().any(|x| x.id == id) {
                scopes.push(SearchScope {
                    id,
                    name: s.info().connection_name.clone(),
                    selected: true,
                });
            }
        }
        let this = cx.entity().downgrade();
        open_find_object(
            service,
            scopes,
            std::rc::Rc::new(move |choice, window, cx| {
                let _ = this.update(cx, |ws, cx| match choice {
                    FindChoice::Reveal(connection, path) => {
                        ws.reveal_object(connection, path, window, cx);
                    }
                    FindChoice::Open(connection, path) => {
                        ws.open_object_editor(connection, path, None, window, cx);
                    }
                });
            }),
            window,
            cx,
        );
    }

    /// Reveals an object in the navigator (shown and focused).
    pub fn reveal_object(
        &mut self,
        connection: ConnectionId,
        path: ObjectPath,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.navigator_visible = true;
        self.navigator.update(cx, |n, cx| {
            n.focus_tree(window, cx);
            n.reveal(connection, path, cx);
        });
        cx.notify();
    }

    /// "Open object editor" on the navigator's selection.
    pub fn open_selected_object(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.navigator_selection(cx) {
            Some(Selection::Object(connection, info)) => {
                let path = info.path.clone();
                self.open_object_editor(connection, path, Some(info), window, cx);
            }
            _ => Self::notify("Select an object in the navigator first.", window, cx),
        }
    }

    /// Opens (or activates) the object editor of an object in the active pane.
    pub fn open_object_editor(
        &mut self,
        connection: ConnectionId,
        path: ObjectPath,
        info: Option<ObjectInfo>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let existing = self.objects.iter().find_map(|(tab, (e, _))| {
            let e = e.read(cx);
            (e.connection() == connection && *e.path() == path).then_some(*tab)
        });
        if let Some(tab) = existing {
            return self.activate_tab(tab, window, cx);
        }
        let hub = self.hub.clone();
        let editor = cx.new(|cx| ObjectEditor::new(hub, connection, path, info, window, cx));
        let sub = cx.subscribe_in(&editor, window, Self::on_object_editor_event);
        let id = self.layout.allocate_tab();
        self.objects.insert(id, (editor, sub));
        self.layout.open_tab(id);
        self.after_change(window, cx);
    }

    /// Opens a new SQL editor on an open session of `connection` with `text` (never run).
    pub fn open_sql_tab(
        &mut self,
        title: String,
        connection: ConnectionId,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<TabId> {
        let session = self.hub.read(cx).sessions(connection).first().cloned()?;
        let id = self.layout.allocate_tab();
        let hub = self.hub.clone();
        let editor = cx.new(|cx| SqlEditorTab::new(title, hub, Some(session), window, cx));
        let inner = editor.read(cx).editor().clone();
        let text = text.to_owned();
        inner.update(cx, |e, cx| {
            e.set_text(&text, window, cx);
        });
        self.tabs.insert(id, editor);
        self.layout.open_tab(id);
        self.active_connection = Some(connection);
        self.after_change(window, cx);
        Some(id)
    }

    /// "Generate SQL" on the target object.
    pub fn generate_selected(
        &mut self,
        which: Template,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((connection, path, _)) = self.target_object(cx) else {
            return Self::notify(
                "Select a table or a view in the navigator first.",
                window,
                cx,
            );
        };
        self.generate(connection, path, which, window, cx);
    }

    /// Opens a template for `path` in a new SQL editor, quoted with the session dialect. The
    /// columns are loaded first when the driver lists them and they are not cached.
    pub fn generate(
        &mut self,
        connection: ConnectionId,
        path: ObjectPath,
        which: Template,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(kind) = path.kind() else {
            return;
        };
        if !which.applies_to(kind) {
            return Self::notify(
                format!(
                    "{} is not offered for a {}.",
                    which.label(),
                    kind_singular(kind)
                ),
                window,
                cx,
            );
        }
        let hub = self.hub.read(cx);
        let (Some(service), true) = (hub.metadata(), hub.is_open(connection)) else {
            return Self::notify(
                "Connect first: the template is quoted for the session's engine.",
                window,
                cx,
            );
        };
        if which.writes() && self.session_is_read_only(connection, cx) {
            return Self::notify(READ_ONLY_TEMPLATES, window, cx);
        }
        if !names_survive_editor(&path, &TemplateInput::none()) {
            return Self::notify(UNSAFE_NAMES, window, cx);
        }
        let kinds = service.child_kinds(connection, &path).unwrap_or_default();
        let columns = kinds
            .contains(&ObjectKind::Column)
            .then(|| service.children(connection, NodeKey::new(path.clone(), ObjectKind::Column)));
        let constraints = kinds.contains(&ObjectKind::Constraint).then(|| {
            service.children(
                connection,
                NodeKey::new(path.clone(), ObjectKind::Constraint),
            )
        });
        cx.spawn_in(window, async move |this, cx| {
            let columns = match columns {
                Some(r) => Some(r.await),
                None => None,
            };
            let constraints = match constraints {
                Some(r) => Some(r.await),
                None => None,
            };
            let input = match (&columns, &constraints) {
                // A list that failed to load is not trusted (M3).
                (Some(Err(_)), _) => TemplateInput {
                    columns: Vec::new(),
                    columns_complete: false,
                    key: KeySource::Unknown,
                },
                (cols, cons) => {
                    let cols = cols.as_ref().and_then(|r| r.as_ref().ok());
                    let mut input = TemplateInput::from_nodes(
                        cols.map(AsRef::as_ref),
                        cons.as_ref()
                            .and_then(|r| r.as_ref().ok())
                            .map(AsRef::as_ref),
                    );
                    if matches!(cons, Some(Err(_))) && !matches!(input.key, KeySource::Known(_)) {
                        input.key = KeySource::Unknown;
                    }
                    input
                }
            };
            let _ = this.update_in(cx, |ws, window, cx| {
                ws.open_template(connection, &path, which, &input, window, cx);
            });
        })
        .detach();
    }

    /// Whether the open session of `connection` is read-only.
    fn session_is_read_only(&self, connection: ConnectionId, cx: &App) -> bool {
        self.hub
            .read(cx)
            .sessions(connection)
            .first()
            .is_some_and(|s| s.info().read_only)
    }

    fn open_template(
        &mut self,
        connection: ConnectionId,
        path: &ObjectPath,
        which: Template,
        input: &TemplateInput,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.hub.read(cx).sessions(connection).first().cloned() else {
            return Self::notify("The connection is not open any more.", window, cx);
        };
        if which.writes() && session.info().read_only {
            return Self::notify(READ_ONLY_TEMPLATES, window, cx);
        }
        // The editor strips or rewrites control characters and line separators: a template
        // with such a name would target another object than the one shown (M2).
        if !names_survive_editor(path, input) {
            return Self::notify(UNSAFE_NAMES, window, cx);
        }
        let dialect = session.dialect();
        let text = template(which, path, input, &|i: &str| dialect.quote_identifier(i));
        let name = path
            .leaf()
            .map(|s| crate::browse::display_name(&s.name))
            .unwrap_or_default();
        self.open_sql_tab(
            format!("{} {name}", which.label()),
            connection,
            &text,
            window,
            cx,
        );
    }

    /// "Show DDL" on the target object: its DDL in a new SQL editor, when the driver declares
    /// DDL for its kind (I8).
    pub fn show_selected_ddl(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((connection, path, _)) = self.target_object(cx) else {
            return Self::notify("Select an object in the navigator first.", window, cx);
        };
        self.with_ddl(
            connection,
            path,
            window,
            cx,
            |ws, connection, path, text, window, cx| {
                if ninesql_editor::hygiene::needs_sanitizing(text) {
                    // The editor would change it: show it read-only, unchanged (M2).
                    ws.open_object_editor(connection, path.clone(), None, window, cx);
                    if let Some(editor) = ws.active_object_editor().cloned() {
                        editor.update(cx, |e, cx| {
                            e.activate(crate::browse::object_editor::ObjectTab::Ddl, window, cx);
                        });
                    }
                    return Self::notify(UNSAFE_DDL, window, cx);
                }
                let name = path
                    .leaf()
                    .map(|s| crate::browse::display_name(&s.name))
                    .unwrap_or_default();
                ws.open_sql_tab(format!("DDL {name}"), connection, text, window, cx);
            },
        );
    }

    /// "Format SQL" in the active SQL editor.
    pub fn format_sql(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.active_editor().cloned() else {
            return Self::notify("Open a SQL editor first.", window, cx);
        };
        tab.update(cx, |t, cx| t.format_sql(window, cx));
    }

    /// "Insert snippet": reads the project's snippets off the UI thread, then the picker.
    pub fn insert_snippet(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.active_editor().cloned() else {
            return Self::notify("Open a SQL editor first.", window, cx);
        };
        let Some(paths) = self.hub.read(cx).project_paths() else {
            return Self::notify(
                "No project is open: snippets are kept per project.",
                window,
                cx,
            );
        };
        let engine = tab
            .read(cx)
            .session()
            .map(|s| s.info().capabilities.engine.as_str().to_owned());
        let loaded = cx.background_spawn(async move { ninesql_core::SnippetStore::load(&paths) });
        cx.spawn_in(window, async move |_, cx| {
            let result = loaded.await;
            let _ = cx.update(|window, cx| match result {
                Ok((store, _dropped)) => {
                    let list: Vec<ninesql_core::Snippet> =
                        store.list(engine.as_deref()).into_iter().cloned().collect();
                    let tab = tab.clone();
                    let on_pick: crate::snippets::OnPick = Rc::new(move |snippet, window, cx| {
                        let tab = tab.clone();
                        let on_expand: crate::snippets::OnExpand =
                            Rc::new(move |expansion, window, cx| {
                                tab.update(cx, |t, cx| {
                                    t.insert_snippet_text(
                                        &expansion.text,
                                        expansion.cursor,
                                        window,
                                        cx,
                                    );
                                });
                            });
                        crate::snippets::open_placeholder_prompt(snippet, on_expand, window, cx);
                    });
                    crate::snippets::open_snippet_picker(list, on_pick, window, cx);
                }
                Err(e) => Self::notify(
                    format!(
                        "The snippets could not be read: {}",
                        crate::browse::display_message(&e.to_string())
                    ),
                    window,
                    cx,
                ),
            });
        })
        .detach();
    }

    /// "Export results…": the active result's statement, run again into a file.
    pub fn export_results(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.active_editor().cloned() else {
            return Self::notify("Open a SQL editor and run a query first.", window, cx);
        };
        let (session, text) = {
            let t = tab.read(cx);
            (t.session().cloned(), t.export_statement(cx))
        };
        let Some(session) = session else {
            return Self::notify("The editor has no open connection.", window, cx);
        };
        let text = match text {
            Ok(t) => t,
            Err(why) => return Self::notify(why, window, cx),
        };
        let Some(transfer) = self.hub.read(cx).transfer() else {
            return Self::notify("The driver runtime is not running.", window, cx);
        };
        if let Some(why) = crate::transfer::session_problem(&session, false) {
            return Self::notify(why, window, cx);
        }
        crate::transfer::export::open_export_dialog(
            session,
            transfer,
            crate::transfer::export::ExportTarget::Statement { text },
            window,
            cx,
        );
    }

    /// The open session and the transfer service for a navigator table, or a notification.
    fn transfer_target(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<(
        ConnectionId,
        ObjectPath,
        ninesql_core::SessionHandle,
        ninesql_core::TransferService,
    )> {
        let Some((connection, path, _)) = self.target_object(cx) else {
            Self::notify("Select a table in the navigator first.", window, cx);
            return None;
        };
        if !path.kind().is_some_and(is_table_like) {
            Self::notify("Select a table or a view in the navigator.", window, cx);
            return None;
        }
        let Some(session) = self.transfer_session(connection, cx) else {
            Self::notify("Connect first.", window, cx);
            return None;
        };
        let hub = self.hub.read(cx);
        let Some(transfer) = hub.transfer() else {
            Self::notify("The driver runtime is not running.", window, cx);
            return None;
        };
        Some((connection, path, session, transfer))
    }

    /// The open session of `connection` an "Export table…" or "Import CSV…" leases
    /// ([`pick_transfer_session`]): the active editor's session when it is on that connection
    /// and free, else the newest free session other than the oldest one (the metadata service
    /// binds the oldest, so a long transfer there would stop every navigator load), else the
    /// oldest when free. When every session is busy, the active editor's (or the newest) is
    /// returned and the dialog says why it cannot start. A session is busy when it is closed,
    /// leased, or an editor tab of this window has a run on it (a paused, paged result
    /// included). Opening a dedicated session is not done: it goes through the interactive
    /// connect flow (secrets, init SQL and host-key confirmations).
    #[must_use]
    pub fn transfer_session(
        &self,
        connection: ConnectionId,
        cx: &App,
    ) -> Option<ninesql_core::SessionHandle> {
        let sessions = self.hub.read(cx).sessions(connection);
        let running: Vec<ninesql_core::SessionId> = self
            .tabs
            .values()
            .filter_map(|t| {
                let t = t.read(cx);
                if !t.is_running() {
                    return None;
                }
                t.run_session().or(t.session()).map(|s| s.id())
            })
            .collect();
        let active = self
            .active_editor()
            .and_then(|t| t.read(cx).session().map(|s| s.id()));
        let ids: Vec<ninesql_core::SessionId> = sessions.iter().map(|s| s.id()).collect();
        let busy = |id: ninesql_core::SessionId| {
            running.contains(&id)
                || sessions
                    .iter()
                    .find(|s| s.id() == id)
                    .is_none_or(|s| s.is_closed() || s.is_leased())
        };
        let id = pick_transfer_session(&ids, active, busy)?;
        sessions.iter().find(|s| s.id() == id).cloned()
    }

    /// "Export table…" on the navigator's table.
    pub fn export_table(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((_, path, session, transfer)) = self.transfer_target(window, cx) else {
            return;
        };
        if let Some(why) = crate::transfer::session_problem(&session, false) {
            return Self::notify(why, window, cx);
        }
        let parts = crate::transfer::table_parts(&path);
        crate::transfer::export::open_export_dialog(
            session,
            transfer,
            crate::transfer::export::ExportTarget::Table { parts },
            window,
            cx,
        );
    }

    /// "Import CSV…" into the navigator's table: its columns are read from the metadata cache
    /// (loaded when needed), then the dialog opens.
    pub fn import_csv(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((connection, path, session, transfer)) = self.transfer_target(window, cx) else {
            return;
        };
        if path.kind() != Some(ObjectKind::Table) {
            return Self::notify("Rows can be imported into a table only.", window, cx);
        }
        if let Some(why) = crate::transfer::session_problem(&session, true) {
            return Self::notify(why, window, cx);
        }
        let Some(service) = self.hub.read(cx).metadata() else {
            return;
        };
        let columns = service.children(connection, NodeKey::new(path.clone(), ObjectKind::Column));
        cx.spawn_in(window, async move |_, cx| {
            let columns = columns.await;
            let _ = cx.update(|window, cx| {
                let targets = match columns {
                    Ok(node) if node.complete && node.hidden_by_filter == 0 => {
                        crate::transfer::import::targets_from(&node.items)
                    }
                    Ok(_) => {
                        return Self::notify(
                            "The table's columns are not all listed (a navigator filter hides some, or the list is incomplete): the import needs every column.",
                            window,
                            cx,
                        );
                    }
                    Err(e) => {
                        return Self::notify(
                            format!(
                                "The table's columns could not be read: {}",
                                crate::browse::display_message(&e.to_string())
                            ),
                            window,
                            cx,
                        );
                    }
                };
                crate::transfer::import::open_import_dialog(
                    session,
                    transfer,
                    crate::transfer::table_parts(&path),
                    Some(path.clone()),
                    targets,
                    window,
                    cx,
                );
            });
        })
        .detach();
    }

    /// "Open table data": the selected table's rows in a new tab, editable when its key allows.
    pub fn open_table_data(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((connection, path, _)) = self.target_object(cx) else {
            return Self::notify("Select a table in the navigator first.", window, cx);
        };
        if !path.kind().is_some_and(is_table_like) {
            return Self::notify("Select a table or a view in the navigator.", window, cx);
        }
        let Some(session) = self.hub.read(cx).sessions(connection).first().cloned() else {
            return Self::notify("Connect first.", window, cx);
        };
        self.active_connection = Some(connection);
        self.open_table_tab(
            session,
            crate::editor_tab::table::TableSource::of_path(path),
            window,
            cx,
        );
    }

    /// Opens a new tab on `session` showing `source`.
    pub fn open_table_tab(
        &mut self,
        session: ninesql_core::SessionHandle,
        source: crate::editor_tab::table::TableSource,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> TabId {
        let id = self.layout.allocate_tab();
        let hub = self.hub.clone();
        let title = source.label();
        let editor = cx.new(|cx| SqlEditorTab::new(title, hub, Some(session), window, cx));
        self.tabs.insert(id, editor.clone());
        self.layout.open_tab(id);
        self.after_change(window, cx);
        editor.update(cx, |t, cx| t.open_table(source, window, cx));
        id
    }

    /// The editing, sort and filter actions of the active editor's table.
    fn table_action(
        &mut self,
        any: &dyn std::any::Any,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let known = any.is::<EditCell>()
            || any.is::<SetCellNull>()
            || any.is::<InsertRow>()
            || any.is::<DuplicateRow>()
            || any.is::<DeleteRows>()
            || any.is::<RevertRowChanges>()
            || any.is::<DiscardChanges>()
            || any.is::<PreviewChanges>()
            || any.is::<SaveChanges>()
            || any.is::<FilterTableRows>()
            || any.is::<FilterTableBySelectedValue>()
            || any.is::<SortTableAscending>()
            || any.is::<SortTableDescending>()
            || any.is::<ClearTableFilter>()
            || any.is::<RefreshTableData>();
        if !known {
            return false;
        }
        let Some(tab) = self.active_editor().cloned() else {
            Self::notify("Open a table first (Open table data).", window, cx);
            return true;
        };
        if any.is::<EditCell>() {
            if let Some((row, col, name, text)) = tab.update(cx, |t, cx| t.cell_to_edit(cx)) {
                crate::editor_tab::edit_dialogs::open_cell_editor(
                    tab, row, col, name, text, window, cx,
                );
            }
        } else if any.is::<SetCellNull>() {
            tab.update(cx, |t, cx| t.set_focused_null(cx));
        } else if any.is::<InsertRow>() {
            let columns = tab.read(cx).insert_columns(cx);
            if columns.is_empty() {
                let why = tab
                    .read(cx)
                    .read_only_reason()
                    .unwrap_or("No column of the result can be set.")
                    .to_owned();
                Self::notify(why, window, cx);
            } else {
                crate::editor_tab::edit_dialogs::open_insert_row(tab, columns, window, cx);
            }
        } else if any.is::<DuplicateRow>() {
            tab.update(cx, |t, cx| t.duplicate_focused_row(cx));
        } else if any.is::<DeleteRows>() {
            tab.update(cx, |t, cx| t.delete_selected_rows(cx));
        } else if any.is::<RevertRowChanges>() {
            tab.update(cx, |t, cx| t.revert_selected_rows(cx));
        } else if any.is::<DiscardChanges>() {
            tab.update(cx, |t, cx| t.discard_changes(cx));
        } else if any.is::<PreviewChanges>() {
            if let Ok(plan) = tab.update(cx, |t, cx| t.plan_changes(cx)) {
                crate::editor_tab::edit_dialogs::open_preview(tab, plan, window, cx);
            }
        } else if any.is::<SaveChanges>() {
            tab.update(cx, |t, cx| t.save_changes(window, cx));
        } else if any.is::<FilterTableRows>() {
            let (has_source, current) = {
                let t = tab.read(cx);
                let s = t.table_state().source.as_ref();
                (s.is_some(), s.and_then(|s| s.query.raw_where.clone()))
            };
            if !has_source {
                Self::notify(
                    "Server-side filters apply to a table opened from the navigator (Open table data).",
                    window,
                    cx,
                );
            } else {
                let history = tab.update(cx, |t, cx| t.filter_history(cx));
                crate::editor_tab::edit_dialogs::open_filter_dialog(
                    tab, current, history, window, cx,
                );
            }
        } else if any.is::<FilterTableBySelectedValue>() {
            tab.update(cx, |t, cx| t.filter_server_by_value(window, cx));
        } else if any.is::<SortTableAscending>() {
            tab.update(cx, |t, cx| t.sort_server(false, window, cx));
        } else if any.is::<SortTableDescending>() {
            tab.update(cx, |t, cx| t.sort_server(true, window, cx));
        } else if any.is::<ClearTableFilter>() {
            tab.update(cx, |t, cx| t.clear_server_filter(window, cx));
        } else if any.is::<RefreshTableData>() {
            tab.update(cx, |t, cx| {
                t.run_source(window, cx);
            });
        }
        true
    }

    /// "Open referenced rows" / "Open referencing rows": the focused row's foreign-key targets,
    /// one opened at once or picked from a list, each in a new tab.
    pub fn open_foreign_key_rows(
        &mut self,
        referencing: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.active_editor().cloned() else {
            return Self::notify("Open a table first (Open table data).", window, cx);
        };
        let Some(session) = tab.read(cx).table_session() else {
            return Self::notify("The editor has no open connection.", window, cx);
        };
        let targets = tab.update(cx, |t, cx| t.foreign_key_targets(referencing, cx));
        let this = cx.entity().downgrade();
        cx.spawn_in(window, async move |_, cx| {
            let result = targets.await;
            let _ = cx.update(|window, cx| match result {
                Ok(mut list) if list.len() == 1 => {
                    let target = list.remove(0);
                    let _ = this.update(cx, |ws, cx| {
                        ws.open_fk_target(session.clone(), target, window, cx);
                    });
                }
                Ok(list) => {
                    let on_pick: crate::editor_tab::edit_dialogs::OnTarget = {
                        let this = this.clone();
                        let session = session.clone();
                        std::rc::Rc::new(move |target, window, cx| {
                            let _ = this.update(cx, |ws, cx| {
                                ws.open_fk_target(session.clone(), target, window, cx);
                            });
                        })
                    };
                    crate::editor_tab::edit_dialogs::open_fk_picker(
                        if referencing {
                            "Open referencing rows"
                        } else {
                            "Open referenced rows"
                        },
                        list,
                        on_pick,
                        window,
                        cx,
                    );
                }
                Err(why) => Self::notify(why, window, cx),
            });
        })
        .detach();
    }

    /// Opens a foreign-key target in a new tab (editable again: a table query of the core).
    pub fn open_fk_target(
        &mut self,
        session: ninesql_core::SessionHandle,
        target: crate::editor_tab::table::FkTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> TabId {
        let mut source = crate::editor_tab::table::TableSource::of_path(target.path);
        source.initial = Some(target.query);
        self.open_table_tab(session, source, window, cx)
    }

    /// "Export connections to DBeaver…".
    pub fn export_to_dbeaver(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let hub = self.hub.read(cx);
        let Some(core) = hub.core().cloned() else {
            return Self::notify(
                "The application core is not running: nothing can be exported.",
                window,
                cx,
            );
        };
        let connections: Vec<(ConnectionId, String)> = hub
            .connections()
            .iter()
            .map(|c| (c.id, c.name.clone()))
            .collect();
        crate::dbeaver_export::open_dbeaver_export(Some(core), connections, window, cx);
    }

    /// "Copy DDL": the active object editor's DDL, else the selected object's.
    pub fn copy_object_ddl(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.navigator.focus_handle(cx).contains_focused(window, cx)
            && let Some(editor) = self.active_object_editor().cloned()
            && editor.read(cx).ddl_text().is_some()
        {
            editor.update(cx, |e, cx| e.copy_ddl(cx));
            return Self::notify("DDL copied.", window, cx);
        }
        let Some((connection, path, _)) = self.target_object(cx) else {
            return Self::notify("Select an object in the navigator first.", window, cx);
        };
        self.with_ddl(connection, path, window, cx, |_, _, _, text, window, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(text.to_owned()));
            Self::notify("DDL copied.", window, cx);
        });
    }

    fn with_ddl(
        &mut self,
        connection: ConnectionId,
        path: ObjectPath,
        window: &mut Window,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, ConnectionId, &ObjectPath, &str, &mut Window, &mut Context<Self>)
        + 'static,
    ) {
        let hub = self.hub.read(cx);
        let Some(service) = hub.metadata() else {
            return;
        };
        let kind = path.kind();
        if !hub.is_open(connection) || !kind.is_some_and(|k| service.supports_ddl(connection, k)) {
            return Self::notify(
                "DDL is not available for this object on this engine (or the connection is not open).",
                window,
                cx,
            );
        }
        if let Some(d) = service.cached_ddl(connection, &path) {
            let text = d.text.clone();
            return then(self, connection, &path, &text, window, cx);
        }
        let request = service.ddl(connection, path.clone());
        cx.spawn_in(window, async move |this, cx| {
            let result = request.await;
            let _ = this.update_in(cx, |ws, window, cx| match result {
                Ok(d) => then(ws, connection, &path, &d.text, window, cx),
                Err(e) => window.push_notification(
                    Notification::error(crate::browse::display_message(&e.to_string()))
                        .title("DDL not loaded"),
                    cx,
                ),
            });
        })
        .detach();
    }

    /// Whether "Generate SQL" applies to the navigator's selection (table-like objects).
    #[must_use]
    pub fn selection_is_table_like(&self, cx: &App) -> bool {
        matches!(
            self.navigator_selection(cx),
            Some(Selection::Object(_, info)) if info.path.kind().is_some_and(is_table_like)
        )
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
            ChooseEditorConnection,
            CommitTransaction,
            RollbackTransaction,
            RefreshNavigatorNode,
            LoadMoreObjects,
            ConfigureNavigatorFilters,
            FindObject,
            OpenObjectEditor,
            GenerateSelectSql,
            GenerateInsertSql,
            GenerateUpdateSql,
            GenerateDeleteSql,
            ShowObjectDdl,
            CopyObjectDdl,
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

/// Which of a connection's open sessions (`sessions`, oldest first) a navigator transfer
/// leases: `active` (the active editor's session) when it is one of them and not `busy`; else
/// the newest free session other than the oldest (which the metadata service binds); else the
/// oldest when free; else `active` when it is one of them, or the newest (the dialog then says
/// the session is busy). `None` when there is no session.
pub fn pick_transfer_session<T: Copy + PartialEq>(
    sessions: &[T],
    active: Option<T>,
    busy: impl Fn(T) -> bool,
) -> Option<T> {
    let active = active.filter(|a| sessions.contains(a));
    if let Some(a) = active.filter(|a| !busy(*a)) {
        return Some(a);
    }
    if let Some(s) = sessions.iter().skip(1).rev().find(|s| !busy(**s)) {
        return Some(*s);
    }
    if let Some(s) = sessions.first().filter(|s| !busy(**s)) {
        return Some(*s);
    }
    active.or_else(|| sessions.last().copied())
}

#[cfg(test)]
mod transfer_session_tests {
    use super::pick_transfer_session;

    #[test]
    fn active_editor_session_first_then_not_the_metadata_one() {
        let free = |_: u32| false;
        // The active editor's session, even the oldest, when free.
        assert_eq!(pick_transfer_session(&[1, 2, 3], Some(1), free), Some(1));
        assert_eq!(pick_transfer_session(&[1, 2, 3], Some(2), free), Some(2));
        // An active editor on another connection does not count: newest, not the oldest.
        assert_eq!(pick_transfer_session(&[1, 2, 3], Some(9), free), Some(3));
        assert_eq!(pick_transfer_session(&[1, 2, 3], None, free), Some(3));
        // One session only: that one.
        assert_eq!(pick_transfer_session(&[1], None, free), Some(1));
        assert_eq!(pick_transfer_session::<u32>(&[], Some(1), free), None);
    }

    #[test]
    fn busy_sessions_are_skipped_and_the_last_resort_is_reported_busy() {
        // The active editor pages a result on 2: the other non-metadata session.
        assert_eq!(
            pick_transfer_session(&[1, 2, 3], Some(2), |s| s == 2),
            Some(3)
        );
        // Every session but the oldest busy: the oldest.
        assert_eq!(
            pick_transfer_session(&[1, 2, 3], Some(2), |s| s != 1),
            Some(1)
        );
        // All busy: the active editor's, so the dialog says why.
        assert_eq!(
            pick_transfer_session(&[1, 2, 3], Some(2), |_| true),
            Some(2)
        );
        assert_eq!(pick_transfer_session(&[1, 2, 3], None, |_| true), Some(3));
    }
}
