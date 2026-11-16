//! Every user-facing action of the shell, with its default key bindings and its command palette
//! entry (F-WS-02, AGENTS.md "Conventions – UI").
//!
//! [`ACTIONS`] holds the shell's own actions (`ninesql::`); [`registry`] merges them with the SQL
//! editor's ([`ninesql_editor::EDITOR_ACTIONS`], `ninesql_editor::`, key context
//! `SqlEditor > Input`) and the results grid's ([`ninesql_grid::actions::GRID_ACTIONS`],
//! `ninesql_grid::`, key context `ResultGrid`). The keymap, the command palette, the
//! `[keybindings]` overrides and the tests all read [`registry`], so an action cannot exist
//! without a binding and a palette entry.

use std::sync::LazyLock;

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
        /// Open the dialog to create a connection.
        NewConnection,
        /// Edit the connection selected in the navigator.
        EditConnection,
        /// Duplicate the connection selected in the navigator.
        DuplicateConnection,
        /// Delete the connection selected in the navigator (asks for confirmation).
        DeleteConnection,
        /// Open a session on the connection selected in the navigator.
        ConnectConnection,
        /// Close the sessions of the connection selected in the navigator.
        DisconnectConnection,
        /// Test the connection selected in the navigator.
        TestConnection,
        /// List the problems found while loading the connections.
        ShowConnectionProblems,
        /// Lock the vault and forget the passwords asked for this session.
        LockSecrets,
        /// Import connections from the DBeaver workspaces found in the known locations.
        ImportFromDbeaver,
        /// Import connections from a DBeaver workspace or project folder chosen by the user.
        ImportFromDbeaverFolder,
        /// Enable the disabled connection selected in the navigator (shows why it was disabled).
        EnableConnection,
        /// Choose the open connection the active SQL editor runs on.
        ChooseEditorConnection,
        /// Commit the open transaction of the active SQL editor's session.
        CommitTransaction,
        /// Roll back the open transaction of the active SQL editor's session.
        RollbackTransaction,
        /// Reload the navigator node selected (a group, an object's group, or a connection).
        RefreshNavigatorNode,
        /// Load the next objects of the navigator group selected.
        LoadMoreObjects,
        /// Edit the navigator filters of the selected connection.
        ConfigureNavigatorFilters,
        /// Search the loaded objects of the open connections.
        FindObject,
        /// Open the object selected in the navigator in the object editor.
        OpenObjectEditor,
        /// Open a SELECT template for the selected table or view in a new SQL editor.
        GenerateSelectSql,
        /// Open an INSERT template for the selected table or view in a new SQL editor.
        GenerateInsertSql,
        /// Open an UPDATE template (with a WHERE placeholder) in a new SQL editor.
        GenerateUpdateSql,
        /// Open a DELETE template (with a WHERE placeholder) in a new SQL editor.
        GenerateDeleteSql,
        /// Open the DDL of the selected object in a new SQL editor.
        ShowObjectDdl,
        /// Copy the DDL of the active object editor (or of the selected object).
        CopyObjectDdl,
    ]
);

/// Where an action is handled, which is also the key context of its bindings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Application-wide: bound without a key context, handled by a global listener.
    App,
    /// Per window: bound in the [`WORKSPACE_CONTEXT`] key context, handled by the workspace.
    Workspace,
    /// The SQL editor's actions (`ninesql_editor::`), bound under
    /// [`ninesql_editor::EDITOR_BINDING_CONTEXT`] and handled by the focused editor.
    Editor,
    /// The results grid's actions (`ninesql_grid::`), bound in
    /// [`ninesql_grid::actions::GRID_CONTEXT`] and handled by the focused results view.
    Grid,
}

/// Prefixes of the action names NineSQL registers (shell, editor, grid).
pub const ACTION_PREFIXES: &[&str] = &["ninesql::", "ninesql_editor::", "ninesql_grid::"];

/// Whether `name` is the name of one of NineSQL's actions (as opposed to a widget's).
#[must_use]
pub fn is_ours(name: &str) -> bool {
    ACTION_PREFIXES.iter().any(|p| name.starts_with(p))
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
            Self::Editor => Some(ninesql_editor::EDITOR_BINDING_CONTEXT),
            Self::Grid => Some(ninesql_grid::actions::GRID_CONTEXT),
        }
    }
}

/// One registered action.
#[derive(Clone, Copy)]
pub struct ActionSpec {
    /// GPUI action name (`ninesql::<Type>`, `ninesql_editor::<Type>`, `ninesql_grid::<Type>`);
    /// the key of `[keybindings]` in settings.toml.
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
    spec!(
        NewConnection,
        "New connection",
        "Connection",
        Workspace,
        ["ctrl-alt-n"],
        keywords = ["database", "add", "create", "datasource"]
    ),
    spec!(
        EditConnection,
        "Edit connection",
        "Connection",
        Workspace,
        ["f4"],
        keywords = ["properties", "settings", "configure"]
    ),
    spec!(
        DuplicateConnection,
        "Duplicate connection",
        "Connection",
        Workspace,
        ["ctrl-k ctrl-d"],
        keywords = ["copy", "clone"]
    ),
    spec!(
        DeleteConnection,
        "Delete connection",
        "Connection",
        Workspace,
        ["ctrl-k delete"],
        keywords = ["remove"]
    ),
    spec!(
        ConnectConnection,
        "Connect",
        "Connection",
        Workspace,
        ["ctrl-k ctrl-o"],
        keywords = ["open", "session", "login"]
    ),
    spec!(
        DisconnectConnection,
        "Disconnect",
        "Connection",
        Workspace,
        ["ctrl-k ctrl-x"],
        keywords = ["close", "session"]
    ),
    spec!(
        TestConnection,
        "Test connection",
        "Connection",
        Workspace,
        ["ctrl-alt-t"],
        keywords = ["ping", "check"]
    ),
    spec!(
        ShowConnectionProblems,
        "Show connection problems",
        "Connection",
        Workspace,
        ["ctrl-k ctrl-l"],
        keywords = ["load", "errors", "issues", "removed"]
    ),
    spec!(
        LockSecrets,
        "Lock secrets now",
        "Connection",
        Workspace,
        ["ctrl-alt-l"],
        keywords = ["vault", "password", "keyring", "forget", "master"]
    ),
    spec!(
        ImportFromDbeaver,
        "Import from DBeaver",
        "Connection",
        Workspace,
        ["ctrl-k ctrl-i"],
        keywords = ["dbeaver", "workspace", "migrate", "data-sources"]
    ),
    spec!(
        ImportFromDbeaverFolder,
        "Import from DBeaver: choose folder",
        "Connection",
        Workspace,
        ["ctrl-k ctrl-shift-i"],
        keywords = ["dbeaver", "workspace", "project", "folder", "custom"]
    ),
    spec!(
        EnableConnection,
        "Enable connection",
        "Connection",
        Workspace,
        ["ctrl-k ctrl-shift-e"],
        keywords = ["disabled", "imported", "route", "activate"]
    ),
    spec!(
        ChooseEditorConnection,
        "Choose editor connection",
        "SQL editor",
        Workspace,
        ["ctrl-k ctrl-p"],
        keywords = ["session", "database", "datasource", "active", "switch"]
    ),
    spec!(
        CommitTransaction,
        "Commit transaction",
        "SQL editor",
        Workspace,
        ["ctrl-k ctrl-c"],
        keywords = ["transaction", "save", "manual commit"]
    ),
    spec!(
        RollbackTransaction,
        "Roll back transaction",
        "SQL editor",
        Workspace,
        ["ctrl-k ctrl-r"],
        keywords = ["transaction", "rollback", "undo", "discard"]
    ),
    spec!(
        RefreshNavigatorNode,
        "Refresh navigator node",
        "Navigator",
        Workspace,
        ["f5"],
        keywords = ["reload", "metadata", "tree"]
    ),
    spec!(
        LoadMoreObjects,
        "Load more objects",
        "Navigator",
        Workspace,
        ["ctrl-k ctrl-m"],
        keywords = ["next", "page", "metadata", "tree"]
    ),
    spec!(
        ConfigureNavigatorFilters,
        "Configure navigator filters",
        "Navigator",
        Workspace,
        ["ctrl-k ctrl-f"],
        keywords = ["filter", "include", "exclude", "pattern", "hide"]
    ),
    spec!(
        FindObject,
        "Find object",
        "Navigator",
        Workspace,
        ["ctrl-shift-o"],
        keywords = ["search", "table", "metadata", "go to", "fuzzy"]
    ),
    spec!(
        OpenObjectEditor,
        "Open object editor",
        "Navigator",
        Workspace,
        ["ctrl-k enter"],
        keywords = ["properties", "columns", "indexes", "keys", "ddl"]
    ),
    spec!(
        GenerateSelectSql,
        "Generate SQL: SELECT",
        "Navigator",
        Workspace,
        ["alt-g s"],
        keywords = ["template", "query", "script"]
    ),
    spec!(
        GenerateInsertSql,
        "Generate SQL: INSERT",
        "Navigator",
        Workspace,
        ["alt-g i"],
        keywords = ["template", "script", "dml"]
    ),
    spec!(
        GenerateUpdateSql,
        "Generate SQL: UPDATE",
        "Navigator",
        Workspace,
        ["alt-g u"],
        keywords = ["template", "script", "dml"]
    ),
    spec!(
        GenerateDeleteSql,
        "Generate SQL: DELETE",
        "Navigator",
        Workspace,
        ["alt-g d"],
        keywords = ["template", "script", "dml"]
    ),
    spec!(
        ShowObjectDdl,
        "Show DDL",
        "Navigator",
        Workspace,
        ["alt-g l"],
        keywords = ["create", "definition", "script", "generate"]
    ),
    spec!(
        CopyObjectDdl,
        "Copy DDL",
        "Navigator",
        Workspace,
        ["ctrl-k ctrl-shift-c"],
        keywords = ["create", "definition", "clipboard"]
    ),
];

/// Every action of the application: [`ACTIONS`], then the editor's and the grid's registries,
/// in that order.
#[must_use]
pub fn registry() -> &'static [ActionSpec] {
    static ALL: LazyLock<Vec<ActionSpec>> = LazyLock::new(|| {
        let editor = ninesql_editor::EDITOR_ACTIONS.iter().map(|e| ActionSpec {
            name: e.name,
            label: e.label,
            category: e.category,
            keywords: e.keywords,
            scope: Scope::Editor,
            default_keys: e.default_keys,
            in_palette: true,
            build: e.build,
        });
        let grid = ninesql_grid::actions::GRID_ACTIONS
            .iter()
            .map(|g| ActionSpec {
                name: g.name,
                label: g.label,
                category: g.category,
                keywords: g.keywords,
                scope: Scope::Grid,
                default_keys: g.default_keys,
                in_palette: true,
                build: g.build,
            });
        ACTIONS.iter().copied().chain(editor).chain(grid).collect()
    });
    &ALL
}

/// Looks up an action by its full name (`ninesql::NewEditorTab`,
/// `ninesql_editor::ExecuteStatement`) or, for the shell's own actions, its short name
/// (`NewEditorTab`).
#[must_use]
pub fn find(name: &str) -> Option<&'static ActionSpec> {
    let short = name.strip_prefix("ninesql::").unwrap_or(name);
    registry()
        .iter()
        .find(|spec| spec.name() == name || spec.name().strip_prefix("ninesql::") == Some(short))
}

/// Looks up the registry entry of an action instance.
#[must_use]
pub fn spec_of(action: &dyn Action) -> Option<&'static ActionSpec> {
    let name = action.name();
    registry().iter().find(|spec| spec.name() == name)
}

#[cfg(test)]
mod tests {
    use super::{ACTIONS, CommandPalette, NewEditorTab, Scope, find, is_ours, registry, spec_of};
    use crate::keymap::validate_keys;
    use std::collections::HashSet;

    #[test]
    fn every_action_has_a_valid_default_binding() {
        for spec in registry() {
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
        for spec in registry() {
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
        }
        for spec in registry() {
            assert!(is_ours(spec.name()), "{}", spec.name());
            let expected = match spec.scope {
                Scope::Editor => "ninesql_editor::",
                Scope::Grid => "ninesql_grid::",
                Scope::App | Scope::Workspace => "ninesql::",
            };
            assert!(spec.name().starts_with(expected), "{}", spec.name());
            assert!(seen.insert(spec.name()), "duplicate {}", spec.name());
            assert_eq!((spec.build)().name(), spec.name());
        }
    }

    #[test]
    fn default_bindings_do_not_collide_within_a_scope() {
        let mut seen = HashSet::new();
        for spec in registry() {
            for keys in spec.default_keys {
                // App bindings apply in the workspace too, and workspace bindings inside the
                // editor and the grid (an inner binding would shadow them): compare across all.
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
        assert_eq!(
            find("ninesql_editor::ExecuteStatement").map(|s| s.scope),
            Some(Scope::Editor)
        );
        assert_eq!(
            find("ninesql_grid::FetchAllRows").map(|s| s.scope),
            Some(Scope::Grid)
        );
        assert_eq!(
            spec_of(&ninesql_grid::actions::CopySelection).map(|s| s.label),
            Some("Copy selected cells")
        );
        assert!(
            find("ExecuteStatement").is_none(),
            "short names are the shell's only"
        );
    }

    #[test]
    fn registry_merges_editor_and_grid_actions() {
        assert_eq!(
            registry().len(),
            ACTIONS.len()
                + ninesql_editor::EDITOR_ACTIONS.len()
                + ninesql_grid::actions::GRID_ACTIONS.len()
        );
        for e in ninesql_editor::EDITOR_ACTIONS {
            let spec = find(e.name()).unwrap_or_else(|| panic!("{}", e.name()));
            assert_eq!(spec.scope.key_context(), Some("SqlEditor > Input"));
            assert_eq!(spec.default_keys, e.default_keys);
        }
        for g in ninesql_grid::actions::GRID_ACTIONS {
            let spec = find(g.name()).unwrap_or_else(|| panic!("{}", g.name()));
            assert_eq!(spec.scope.key_context(), Some("ResultGrid"));
        }
    }
}
