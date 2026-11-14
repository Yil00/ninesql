//! Every user-facing action of the shell, with its default key bindings and its command palette
//! entry (F-WS-02, AGENTS.md "Conventions – UI").
//!
//! [`ACTIONS`] is the single registry: the keymap, the command palette and the tests all read it,
//! so an action cannot exist without a binding and a palette entry.

use gpui_kit::{Action, actions};

actions!(
    ninesql,
    [
        /// Quit NineSQL (all windows).
        Quit,
        /// Open a new main window.
        NewWindow,
        /// Close the current window.
        CloseWindow,
        /// Open the command palette.
        CommandPalette,
        /// Open an empty SQL editor tab in the active pane.
        NewEditorTab,
        /// Close the active editor tab.
        CloseTab,
        /// Activate the next tab of the active pane.
        NextTab,
        /// Activate the previous tab of the active pane.
        PreviousTab,
        /// Split the active pane to the right.
        SplitRight,
        /// Split the active pane downwards.
        SplitDown,
        /// Move focus to the next pane.
        FocusNextPane,
        /// Move focus to the previous pane.
        FocusPreviousPane,
        /// Show or hide the database navigator.
        ToggleNavigator,
        /// Move focus to the database navigator.
        FocusNavigator,
        /// Switch between the light and dark themes.
        ToggleTheme,
        /// Make the interface and editor fonts larger.
        IncreaseFontSize,
        /// Make the interface and editor fonts smaller.
        DecreaseFontSize,
        /// Restore the font sizes from the settings.
        ResetFontSize,
        /// List the problems found in settings.toml.
        ShowSettingsProblems,
    ]
);

/// Where an action is handled, which is also the key context of its bindings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Application-wide: bound without a key context, handled by a global listener.
    App,
    /// Per window: bound in the [`WORKSPACE_CONTEXT`] key context, handled by the workspace.
    Workspace,
}

/// Key context set by the workspace root element.
pub const WORKSPACE_CONTEXT: &str = "Workspace";

impl Scope {
    /// Key context of the bindings of this scope.
    #[must_use]
    pub fn key_context(self) -> Option<&'static str> {
        match self {
            Self::App => None,
            Self::Workspace => Some(WORKSPACE_CONTEXT),
        }
    }
}

/// One registered action.
#[derive(Clone, Copy)]
pub struct ActionSpec {
    /// GPUI action name, `ninesql::<Type>`; the key of `[keybindings]` in settings.toml.
    pub name: fn() -> &'static str,
    /// Command palette label.
    pub label: &'static str,
    /// Command palette group.
    pub category: &'static str,
    /// Extra search terms for the palette.
    pub keywords: &'static [&'static str],
    /// Handling scope and key context.
    pub scope: Scope,
    /// Default key bindings, GPUI syntax (`ctrl-k ctrl-t` is a two-key sequence).
    pub default_keys: &'static [&'static str],
    /// Shown in the command palette. Only the palette itself is not listed.
    pub in_palette: bool,
    /// Builds a boxed instance.
    pub build: fn() -> Box<dyn Action>,
}

impl ActionSpec {
    /// The action name.
    #[must_use]
    pub fn name(&self) -> &'static str {
        (self.name)()
    }
}

impl std::fmt::Debug for ActionSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActionSpec")
            .field("name", &self.name())
            .field("scope", &self.scope)
            .field("default_keys", &self.default_keys)
            .finish_non_exhaustive()
    }
}

macro_rules! spec {
    ($ty:ident, $label:literal, $category:literal, $scope:ident, [$($key:literal),+ $(,)?]
     $(, keywords = [$($kw:literal),* $(,)?])? $(, palette = $pal:literal)?) => {
        ActionSpec {
            name: <$ty as Action>::name_for_type,
            label: $label,
            category: $category,
            keywords: &[$($($kw),*)?],
            scope: Scope::$scope,
            default_keys: &[$($key),+],
            in_palette: spec!(@palette $($pal)?),
            build: || Box::new($ty),
        }
    };
    (@palette) => { true };
    (@palette $pal:literal) => { $pal };
}

/// The registry. Default keys follow common Linux editor conventions and avoid the bindings of
/// the SQL editor (gpui-component `Input`), which win inside the editor.
pub const ACTIONS: &[ActionSpec] = &[
    spec!(
        CommandPalette,
        "Show command palette",
        "General",
        Workspace,
        ["ctrl-shift-p", "f1"],
        palette = false
    ),
    spec!(NewWindow, "New window", "Window", App, ["ctrl-shift-n"]),
    spec!(
        CloseWindow,
        "Close window",
        "Window",
        Workspace,
        ["ctrl-shift-w"]
    ),
    spec!(
        Quit,
        "Quit NineSQL",
        "Window",
        App,
        ["ctrl-q"],
        keywords = ["exit"]
    ),
    spec!(
        NewEditorTab,
        "New SQL editor",
        "Editor",
        Workspace,
        ["ctrl-t"],
        keywords = ["script", "tab", "query"]
    ),
    spec!(
        CloseTab,
        "Close editor tab",
        "Editor",
        Workspace,
        ["ctrl-w"]
    ),
    spec!(
        NextTab,
        "Next tab",
        "Editor",
        Workspace,
        ["ctrl-tab", "ctrl-pagedown"]
    ),
    spec!(
        PreviousTab,
        "Previous tab",
        "Editor",
        Workspace,
        ["ctrl-shift-tab", "ctrl-pageup"]
    ),
    spec!(
        SplitRight,
        "Split editor right",
        "Editor",
        Workspace,
        ["ctrl-\\"],
        keywords = ["pane"]
    ),
    spec!(
        SplitDown,
        "Split editor down",
        "Editor",
        Workspace,
        ["ctrl-k ctrl-\\"],
        keywords = ["pane"]
    ),
    spec!(
        FocusNextPane,
        "Focus next pane",
        "Editor",
        Workspace,
        ["ctrl-k ctrl-right"]
    ),
    spec!(
        FocusPreviousPane,
        "Focus previous pane",
        "Editor",
        Workspace,
        ["ctrl-k ctrl-left"]
    ),
    spec!(
        ToggleNavigator,
        "Toggle database navigator",
        "View",
        Workspace,
        ["ctrl-b"],
        keywords = ["sidebar", "dock", "tree"]
    ),
    spec!(
        FocusNavigator,
        "Focus database navigator",
        "View",
        Workspace,
        ["ctrl-k ctrl-n"]
    ),
    spec!(
        ToggleTheme,
        "Toggle light / dark theme",
        "View",
        App,
        ["ctrl-k ctrl-t"],
        keywords = ["appearance", "color"]
    ),
    spec!(
        IncreaseFontSize,
        "Increase font size",
        "View",
        App,
        ["ctrl-=", "ctrl-+"],
        keywords = ["zoom"]
    ),
    spec!(
        DecreaseFontSize,
        "Decrease font size",
        "View",
        App,
        ["ctrl--"],
        keywords = ["zoom"]
    ),
    spec!(
        ResetFontSize,
        "Reset font size",
        "View",
        App,
        ["ctrl-0"],
        keywords = ["zoom"]
    ),
    spec!(
        ShowSettingsProblems,
        "Show settings problems",
        "General",
        Workspace,
        ["ctrl-k ctrl-e"],
        keywords = ["settings.toml", "errors", "keybindings"]
    ),
];

/// Looks up an action by its full name (`ninesql::NewEditorTab`) or its short name
/// (`NewEditorTab`).
#[must_use]
pub fn find(name: &str) -> Option<&'static ActionSpec> {
    let short = name.strip_prefix("ninesql::").unwrap_or(name);
    ACTIONS
        .iter()
        .find(|spec| spec.name() == name || spec.name().strip_prefix("ninesql::") == Some(short))
}

/// Looks up the registry entry of an action instance.
#[must_use]
pub fn spec_of(action: &dyn Action) -> Option<&'static ActionSpec> {
    let name = action.name();
    ACTIONS.iter().find(|spec| spec.name() == name)
}

#[cfg(test)]
mod tests {
    use super::{ACTIONS, CommandPalette, NewEditorTab, Scope, find, spec_of};
    use crate::keymap::validate_keys;
    use std::collections::HashSet;

    #[test]
    fn every_action_has_a_valid_default_binding() {
        for spec in ACTIONS {
            assert!(
                !spec.default_keys.is_empty(),
                "{} has no binding",
                spec.name()
            );
            for keys in spec.default_keys {
                assert!(
                    validate_keys(keys).is_ok(),
                    "{}: invalid default binding {keys}",
                    spec.name()
                );
            }
        }
    }

    #[test]
    fn every_action_but_the_palette_is_in_the_palette() {
        for spec in ACTIONS {
            let expected = spec.name() != "ninesql::CommandPalette";
            assert_eq!(spec.in_palette, expected, "{}", spec.name());
            assert!(!spec.label.is_empty());
        }
    }

    #[test]
    fn names_are_unique_and_namespaced() {
        let mut seen = HashSet::new();
        for spec in ACTIONS {
            assert!(spec.name().starts_with("ninesql::"), "{}", spec.name());
            assert!(seen.insert(spec.name()), "duplicate {}", spec.name());
            assert_eq!((spec.build)().name(), spec.name());
        }
    }

    #[test]
    fn default_bindings_do_not_collide_within_a_scope() {
        let mut seen = HashSet::new();
        for spec in ACTIONS {
            for keys in spec.default_keys {
                // App bindings apply in the workspace too, so compare across both scopes.
                assert!(seen.insert(*keys), "{keys} bound twice");
            }
        }
    }

    #[test]
    fn lookup_by_full_or_short_name() {
        assert_eq!(
            find("ninesql::NewEditorTab").map(|s| s.label),
            Some("New SQL editor")
        );
        assert_eq!(
            find("NewEditorTab").map(|s| s.label),
            Some("New SQL editor")
        );
        assert!(find("editor::NewEditorTab").is_none());
        assert!(find("Nope").is_none());
        assert_eq!(
            spec_of(&NewEditorTab).map(|s| s.scope),
            Some(Scope::Workspace)
        );
        assert_eq!(spec_of(&CommandPalette).map(|s| s.in_palette), Some(false));
    }
}
