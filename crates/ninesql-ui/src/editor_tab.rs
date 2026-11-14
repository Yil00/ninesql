//! An SQL editor tab: gpui-component's `EditorState` with tree-sitter SQL highlighting, as in
//! the P0-A spike. Execution, completion and statement splitting arrive in P3 and will live in
//! `ninesql-editor`.

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::input::{Editor, EditorState};
use gpui_kit::*;

/// One editor tab.
pub struct SqlEditorTab {
    title: SharedString,
    editor: Entity<EditorState>,
}

impl SqlEditorTab {
    /// An empty SQL editor titled `title`.
    pub fn new(
        title: impl Into<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("sql")
                .line_number(true)
        });
        Self {
            title: title.into(),
            editor,
        }
    }

    /// Tab title.
    #[must_use]
    pub fn title(&self) -> &SharedString {
        &self.title
    }

    /// The editor state (text, cursor, highlighting).
    #[must_use]
    pub fn editor(&self) -> &Entity<EditorState> {
        &self.editor
    }
}

impl Focusable for SqlEditorTab {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Render for SqlEditorTab {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        Editor::new(&self.editor)
            .bordered(false)
            .h(relative(1.))
            .font_family(cx.theme().mono_font_family.clone())
            .text_size(cx.theme().mono_font_size)
    }
}
