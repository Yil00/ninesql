//! Results grid (ROADMAP P3-B, F-DATA-01, F-SQL-04): a virtualized GPUI view over Arrow
//! batches, under a memory budget (ADR-0004, invariant I6).
//!
//! # Layers
//! - GPUI-free models, unit-tested and benchmarked headless:
//!   [`format`] (type-aware cell text), [`model`] (one result set: batches, row lookup, paging
//!   state, bytes), [`selection`], [`copy`] (TSV / CSV, bounded), [`execution`] (every result of
//!   one execution: result tabs, update counts, notices, the execution's memory budget and its
//!   policy).
//! - [`source`]: the adapter the execution service implements ([`ResultSource`]) and the events
//!   it sends ([`ExecutionEvent`]). The grid never awaits I/O (I6).
//! - [`view`]: the GPUI entity [`ResultsView`], drawing the active result with `gpui-component`'s
//!   `DataTable` (only visible rows and columns are built), the update counts and notices panes,
//!   the truncation banner and the fetch controls.
//! - [`actions`]: every user-facing action with its default key binding
//!   ([`actions::GRID_ACTIONS`]), for
//!   `ninesql-ui` to register in its keymap and command palette.
#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod actions;
pub mod copy;
pub mod execution;
pub mod format;
pub mod model;
pub mod selection;
pub mod source;
pub mod view;

pub use ninesql_core as core;

pub use copy::{CopyFormat, CopyOptions, CopyOutput, copy_selection};
pub use execution::{ExecutionResults, ExecutionState, GridBudget, ResultTab};
pub use format::{CellText, DisplayOptions, ValueKind};
pub use model::{ColumnMeta, FetchState, ResultSetModel};
pub use selection::{ColumnOrder, GridSelection};
pub use source::{ExecutionEvent, FetchMode, FetchRequest, ResultSource};
pub use view::{ActivePane, GridDelegate, GridEvent, ResultsView};

/// Crate name, for diagnostics.
pub const CRATE: &str = env!("CARGO_PKG_NAME");

/// Installs the grid's default key bindings in the [`actions::GRID_CONTEXT`] key context. Call it
/// after `gpui_kit::init`; `ninesql-ui` may instead install [`actions::GRID_ACTIONS`] through its
/// own keymap, so user overrides apply.
pub fn init(cx: &mut gpui_kit::App) {
    actions::bind_default_keys(cx);
}
