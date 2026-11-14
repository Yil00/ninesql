//! Database navigator dock: an empty tree until connections exist (P1-C) and metadata is
//! browsable (P4).

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::list::ListItem;
use gpui_kit::component::tree::{TreeState, tree};
use gpui_kit::component::v_flex;
use gpui_kit::*;

/// The navigator view.
pub struct Navigator {
    tree: Entity<TreeState>,
    focus: FocusHandle,
}

impl Navigator {
    /// An empty navigator.
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            tree: cx.new(|cx| TreeState::new(cx).items(Vec::new())),
            focus: cx.focus_handle(),
        }
    }

    /// Whether the tree has no entry (always, until P1-C adds connections).
    #[must_use]
    pub fn is_empty(&self, cx: &App) -> bool {
        self.tree.read(cx).entry(0).is_none()
    }
}

impl Focusable for Navigator {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Navigator {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let empty = self.is_empty(cx);
        v_flex()
            .id("navigator")
            .key_context("Navigator")
            .track_focus(&self.focus)
            .size_full()
            .bg(theme.sidebar)
            .text_color(theme.sidebar_foreground)
            .child(
                div()
                    .px_3()
                    .py_2()
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.muted_foreground)
                    .child("DATABASE NAVIGATOR"),
            )
            .child(if empty {
                div()
                    .id("navigator-empty")
                    .px_3()
                    .py_2()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child("No connections yet.")
                    .into_any_element()
            } else {
                div()
                    .flex_1()
                    .child(tree(&self.tree, |ix, entry, selected, _window, _cx| {
                        ListItem::new(ix)
                            .selected(selected)
                            .child(entry.item().label.clone())
                    }))
                    .into_any_element()
            })
    }
}
