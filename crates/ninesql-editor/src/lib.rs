//! SQL editor (ROADMAP P3-A): gpui-component's code editor with tree-sitter SQL highlighting
//! (the MIT `tree-sitter-sequel` grammar), never Zed's GPL editor (ADR-0001, I7).
//!
//! - [`view::SqlEditor`]: the view, with find / replace, multi-cursor, folding (F-SQL-15),
//!   completion (F-SQL-02), execution requests (F-SQL-03), the parameters prompt (F-SQL-06),
//!   error positions (F-SQL-12) and commit-path hygiene (P1-A IME follow-up).
//! - Adapter traits for work owned elsewhere: [`split::StatementSplitter`] (the dialect's
//!   splitting, P3-D), [`execution::ExecutionSink`] (the core's execution service, P3-C),
//!   [`completion::MetadataProvider`] (the core's metadata cache).
//! - [`script`]: script files linked to a connection (F-SQL-11) and session restore
//!   (F-WS-06), as models with a JSON form for the storage layer.
//! - [`actions`]: every action with its default key binding, for the shell's keymap and command
//!   palette.
#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod actions;
pub mod completion;
pub mod edit_ops;
pub mod execution;
pub mod hygiene;
pub mod params;
pub mod script;
pub mod split;
pub mod text;
pub mod view;

pub use ninesql_core as core;

pub use actions::{EDITOR_ACTIONS, EDITOR_BINDING_CONTEXT, EDITOR_CONTEXT, EditorActionSpec};
pub use view::{DirtyChanged, EditorSetup, ShownError, SqlEditor};

/// Installs the editor's default key bindings. Call after `gpui_kit::init`; `ninesql-ui`
/// installs [`EDITOR_ACTIONS`] through its own keymap instead (user overrides).
pub fn init(cx: &mut gpui_kit::App) {
    actions::bind_default_keys(cx);
}

#[cfg(test)]
mod view_tests;
