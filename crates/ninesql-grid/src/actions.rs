//! Every user-facing action of the results grid, with its default key bindings (AGENTS.md
//! "Conventions – UI"). [`GRID_ACTIONS`] is the registry `ninesql-ui` merges into its keymap
//! (user overrides) and its command palette; [`bind_default_keys`] installs the defaults alone.
//!
//! The bindings live in the [`GRID_CONTEXT`] key context, set by the results view, so they never
//! shadow the editor's or the shell's. Arrow keys, Page Up / Down, Home / End and Escape are
//! `gpui-component`'s `DataTable` bindings (cell navigation), active inside the table.

use std::rc::Rc;

use gpui_kit::{Action, App, KeyBinding, KeyBindingContextPredicate, actions};

actions!(
    ninesql_grid,
    [
        /// Copy the selected cells as tab-separated values.
        CopySelection,
        /// Copy the selected cells as tab-separated values, with a line of column names.
        CopySelectionWithHeaders,
        /// Copy the selected cells as comma-separated values, with a line of column names.
        CopySelectionAsCsv,
        /// Select every loaded row and column of the result.
        SelectAllCells,
        /// Extend the selection one row up.
        ExtendSelectionUp,
        /// Extend the selection one row down.
        ExtendSelectionDown,
        /// Extend the selection one column left.
        ExtendSelectionLeft,
        /// Extend the selection one column right.
        ExtendSelectionRight,
        /// Fetch the next page of rows of the active result.
        FetchNextPage,
        /// Fetch every remaining row of the active result, up to the memory budget.
        FetchAllRows,
        /// Fit the selected column's width to its content.
        AutoSizeColumn,
        /// Fit every column's width to its content.
        AutoSizeAllColumns,
        /// Show the next result tab (results, update counts, notices).
        NextResultTab,
        /// Show the previous result tab.
        PreviousResultTab,
    ]
);

/// Key context of the results view.
pub const GRID_CONTEXT: &str = "ResultGrid";

/// One grid action: what `ninesql-ui` needs for its keymap and command palette.
#[derive(Clone, Copy)]
pub struct GridActionSpec {
    /// GPUI action name, `ninesql_grid::<Type>` (its own namespace, so the shell's `ninesql::`
    /// registry stays exact); the key of `[keybindings]` in settings.toml.
    pub name: fn() -> &'static str,
    /// Command palette label.
    pub label: &'static str,
    /// Command palette group.
    pub category: &'static str,
    /// Extra search terms for the palette.
    pub keywords: &'static [&'static str],
    /// Default key bindings, GPUI syntax, in the [`GRID_CONTEXT`] key context.
    pub default_keys: &'static [&'static str],
    /// Builds a boxed instance.
    pub build: fn() -> Box<dyn Action>,
}

impl GridActionSpec {
    /// The action name.
    #[must_use]
    pub fn name(&self) -> &'static str {
        (self.name)()
    }
}

impl std::fmt::Debug for GridActionSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GridActionSpec")
            .field("name", &self.name())
            .field("default_keys", &self.default_keys)
            .finish_non_exhaustive()
    }
}

macro_rules! spec {
    ($ty:ident, $label:literal, [$($key:literal),+ $(,)?] $(, keywords = [$($kw:literal),* $(,)?])?) => {
        GridActionSpec {
            name: <$ty as Action>::name_for_type,
            label: $label,
            category: "Results",
            keywords: &[$($($kw),*)?],
            default_keys: &[$($key),+],
            build: || Box::new($ty),
        }
    };
}

/// The registry. Keys avoid the shell's bindings (`ninesql-ui` `ACTIONS`).
pub const GRID_ACTIONS: &[GridActionSpec] = &[
    spec!(
        CopySelection,
        "Copy selected cells",
        ["ctrl-c"],
        keywords = ["tsv", "clipboard"]
    ),
    spec!(
        CopySelectionWithHeaders,
        "Copy selected cells with column names",
        ["ctrl-shift-c"],
        keywords = ["tsv", "clipboard", "header"]
    ),
    spec!(
        CopySelectionAsCsv,
        "Copy selected cells as CSV",
        ["ctrl-alt-c"],
        keywords = ["csv", "clipboard", "comma"]
    ),
    spec!(
        SelectAllCells,
        "Select all loaded rows",
        ["ctrl-a"],
        keywords = ["grid", "selection"]
    ),
    spec!(ExtendSelectionUp, "Extend selection up", ["shift-up"]),
    spec!(ExtendSelectionDown, "Extend selection down", ["shift-down"]),
    spec!(ExtendSelectionLeft, "Extend selection left", ["shift-left"]),
    spec!(
        ExtendSelectionRight,
        "Extend selection right",
        ["shift-right"]
    ),
    spec!(
        FetchNextPage,
        "Fetch next page of rows",
        ["alt-pagedown"],
        keywords = ["more", "rows", "load"]
    ),
    spec!(
        FetchAllRows,
        "Fetch all rows (up to the memory budget)",
        ["alt-end"],
        keywords = ["more", "rows", "load", "everything"]
    ),
    spec!(
        AutoSizeColumn,
        "Fit column width to content",
        ["alt-r"],
        keywords = ["resize", "width", "auto-size"]
    ),
    spec!(
        AutoSizeAllColumns,
        "Fit all column widths to content",
        ["alt-shift-r"],
        keywords = ["resize", "width", "auto-size"]
    ),
    spec!(
        NextResultTab,
        "Next result tab",
        ["alt-right"],
        keywords = ["notices", "update counts"]
    ),
    spec!(
        PreviousResultTab,
        "Previous result tab",
        ["alt-left"],
        keywords = ["notices", "update counts"]
    ),
];

/// Installs the default bindings of [`GRID_ACTIONS`] in the [`GRID_CONTEXT`] key context.
pub fn bind_default_keys(cx: &mut App) {
    let predicate = KeyBindingContextPredicate::parse(GRID_CONTEXT)
        .ok()
        .map(Rc::new);
    let mut bindings = Vec::new();
    for spec in GRID_ACTIONS {
        for keys in spec.default_keys {
            match KeyBinding::load(keys, (spec.build)(), predicate.clone(), false, None, cx) {
                Ok(binding) => bindings.push(binding),
                Err(_) => tracing::warn!(action = spec.name(), "invalid default key binding"),
            }
        }
    }
    cx.bind_keys(bindings);
}
