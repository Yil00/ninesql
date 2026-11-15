//! Every user-facing action of the SQL editor, with its default key bindings (AGENTS.md
//! "Conventions – UI"). [`EDITOR_ACTIONS`] is the registry `ninesql-ui` merges into its keymap
//! (user overrides) and its command palette; [`bind_default_keys`] installs the defaults alone.
//!
//! The editor view sets the [`EDITOR_CONTEXT`] key context around gpui-component's `Input`.
//! Bindings use the [`EDITOR_BINDING_CONTEXT`] predicate (`SqlEditor > Input`): it matches at
//! the depth of the focused `Input`, so `ctrl-enter`, `ctrl-f` and `ctrl-h`, which the `Input`
//! binds itself (secondary Enter, search, replace), reach the editor's actions instead, and the
//! bindings never apply outside an SQL editor (the results grid uses `ResultGrid`). Editing keys
//! (arrows, undo, copy, add cursor above / below with `ctrl-alt-up` / `ctrl-alt-down`…) stay
//! gpui-component's.

use std::rc::Rc;

use gpui_kit::{Action, App, KeyBinding, KeyBindingContextPredicate, actions};

actions!(
    ninesql_editor,
    [
        /// Run the statement under the cursor (the selection, when there is one).
        ExecuteStatement,
        /// Run the selected statements.
        ExecuteSelection,
        /// Run every statement of the script (the selection, when there is one).
        ExecuteScript,
        /// Cancel the running statement of this editor.
        CancelExecution,
        /// Show completions at the cursor.
        TriggerCompletion,
        /// Comment or uncomment the selected lines with `--`.
        ToggleComment,
        /// Find in the editor.
        Find,
        /// Find and replace in the editor.
        FindReplace,
        /// Add the next occurrence of the selection (or of the word under the cursor) as another
        /// cursor.
        SelectNextOccurrence,
        /// Move the cursor to the position of the last error.
        GoToError,
        /// Select every bidirectional control character of the script, to review or delete them.
        SelectBidiControls,
    ]
);

/// Key context set by the editor view.
pub const EDITOR_CONTEXT: &str = "SqlEditor";

/// Context predicate of the default bindings.
pub const EDITOR_BINDING_CONTEXT: &str = "SqlEditor > Input";

/// One editor action: what `ninesql-ui` needs for its keymap and command palette.
#[derive(Clone, Copy)]
pub struct EditorActionSpec {
    /// GPUI action name, `ninesql_editor::<Type>`; the key of `[keybindings]` in settings.toml.
    pub name: fn() -> &'static str,
    /// Command palette label.
    pub label: &'static str,
    /// Command palette group.
    pub category: &'static str,
    /// Extra search terms for the palette.
    pub keywords: &'static [&'static str],
    /// Default key bindings, GPUI syntax, under [`EDITOR_BINDING_CONTEXT`].
    pub default_keys: &'static [&'static str],
    /// Builds a boxed instance.
    pub build: fn() -> Box<dyn Action>,
}

impl EditorActionSpec {
    /// The action name.
    #[must_use]
    pub fn name(&self) -> &'static str {
        (self.name)()
    }
}

impl std::fmt::Debug for EditorActionSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EditorActionSpec")
            .field("name", &self.name())
            .field("default_keys", &self.default_keys)
            .finish_non_exhaustive()
    }
}

macro_rules! spec {
    ($ty:ident, $label:literal, [$($key:literal),+ $(,)?] $(, keywords = [$($kw:literal),* $(,)?])?) => {
        EditorActionSpec {
            name: <$ty as Action>::name_for_type,
            label: $label,
            category: "SQL editor",
            keywords: &[$($($kw),*)?],
            default_keys: &[$($key),+],
            build: || Box::new($ty),
        }
    };
}

/// The registry. Keys avoid the shell's bindings (`ninesql-ui` `ACTIONS`) and follow DBeaver
/// where it has one (`ctrl-enter`, `alt-x`, `ctrl-space`, `ctrl-/`).
pub const EDITOR_ACTIONS: &[EditorActionSpec] = &[
    spec!(
        ExecuteStatement,
        "Execute statement",
        ["ctrl-enter"],
        keywords = ["run", "query", "current"]
    ),
    spec!(
        ExecuteSelection,
        "Execute selection",
        ["ctrl-shift-enter"],
        keywords = ["run", "selected"]
    ),
    spec!(
        ExecuteScript,
        "Execute script",
        ["alt-x"],
        keywords = ["run", "all", "statements"]
    ),
    spec!(
        CancelExecution,
        "Cancel execution",
        ["ctrl-."],
        keywords = ["stop", "abort", "query"]
    ),
    spec!(
        TriggerCompletion,
        "Show completions",
        ["ctrl-space"],
        keywords = ["autocomplete", "suggest", "assist"]
    ),
    spec!(
        ToggleComment,
        "Toggle line comment",
        ["ctrl-/"],
        keywords = ["comment", "uncomment"]
    ),
    spec!(Find, "Find", ["ctrl-f"], keywords = ["search"]),
    spec!(
        FindReplace,
        "Find and replace",
        ["ctrl-h"],
        keywords = ["search", "replace"]
    ),
    spec!(
        SelectNextOccurrence,
        "Add next occurrence to selection",
        ["ctrl-d"],
        keywords = ["multi-cursor", "cursor", "occurrence"]
    ),
    spec!(
        GoToError,
        "Go to error",
        ["f8"],
        keywords = ["error", "problem", "position"]
    ),
    spec!(
        SelectBidiControls,
        "Select bidirectional control characters",
        ["alt-f8"],
        keywords = ["bidi", "unicode", "trojan", "invisible"]
    ),
];

/// Installs the default bindings of [`EDITOR_ACTIONS`] under [`EDITOR_BINDING_CONTEXT`].
pub fn bind_default_keys(cx: &mut App) {
    let predicate = KeyBindingContextPredicate::parse(EDITOR_BINDING_CONTEXT)
        .ok()
        .map(Rc::new);
    let mut bindings = Vec::new();
    for spec in EDITOR_ACTIONS {
        for keys in spec.default_keys {
            match KeyBinding::load(keys, (spec.build)(), predicate.clone(), false, None, cx) {
                Ok(binding) => bindings.push(binding),
                Err(_) => tracing::warn!(action = spec.name(), "invalid default key binding"),
            }
        }
    }
    cx.bind_keys(bindings);
}
