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
    self, CloseTab, CloseWindow, CommandPalette, FocusNavigator, FocusNextPane, FocusPreviousPane,
    NewEditorTab, NextTab, PreviousTab, Scope, ShowSettingsProblems, SplitDown, SplitRight,
    ToggleNavigator, WORKSPACE_CONTEXT,
};
use crate::appearance;
use crate::editor_tab::SqlEditorTab;
use crate::keymap::EffectiveKeymap;
use crate::layout::{EditorLayout, Node, PaneId, SplitAxis, TabId};
use crate::navigator::Navigator;
use crate::services::Services;
use crate::settings::SettingsState;

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
}

impl Workspace {
    /// A workspace with the navigator and one empty SQL editor.
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            focus: cx.focus_handle(),
            layout: EditorLayout::new(),
            tabs: HashMap::new(),
            navigator: cx.new(Navigator::new),
            navigator_visible: true,
            next_script: 1,
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
        self.navigator.focus_handle(cx).focus(window, cx);
        cx.notify();
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
        let connection = Services::bridge(cx)
            .active_connection_label()
            .unwrap_or_else(|| "No connection".into());
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
        let mut bar = StatusBar::new().left(div().id("status-connection").child(connection));
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
                        .child(resizable_panel().child(editor_area)),
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
