//! Results grid (ROADMAP P3-B, F-DATA-01, F-SQL-04): a virtualized GPUI view over Arrow
//! batches, under a memory budget (ADR-0004, invariant I6).
//!
//! # Layers
//! - GPUI-free models, unit-tested and benchmarked headless:
//!   [`format`] (type-aware cell text), [`model`] (one result set: batches, row lookup, paging
//!   state, bytes), [`selection`], [`copy`] (TSV / CSV, bounded), [`execution`] (every result of
//!   one execution: result tabs, update counts, notices, the execution's memory budget and its
//!   policy).
//! - P5-A models, GPUI-free too: [`rows`] (client-side sort and filter of the loaded rows),
//!   [`aggregate`] (aggregate panel), [`inspect`] (record view, text view, metadata panel),
//!   [`viewer`] (value viewers: text, JSON, XML, hex, image), [`advanced_copy`] (copy as CSV,
//!   TSV, Markdown, JSON, SQL `INSERT`) and [`edit`] (pending edits and the core's
//!   [`RowEditor`] seam; nothing writes yet).
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
pub mod advanced_copy;
pub mod aggregate;
pub mod copy;
pub mod edit;
pub mod execution;
pub mod format;
pub mod inspect;
pub mod model;
pub mod rows;
pub mod selection;
pub mod source;
pub mod view;
pub mod viewer;

pub use ninesql_core as core;

pub use advanced_copy::{AdvancedCopy, AdvancedFormat, CopyError, InsertTarget};
pub use copy::{CopyFormat, CopyOptions, CopyOutput, copy_selection};
pub use edit::{EditBuffer, EditValue, Editability, RowEditor, RowRef};
pub use execution::{ExecutionResults, ExecutionState, GridBudget, ResultTab};
pub use format::{CellText, DisplayOptions, ValueKind};
pub use model::{ColumnMeta, FetchState, ResultSetModel};
pub use rows::{
    ClientFilter, FilterOp, RowView, ServerSortFilter, SortDirection, SortFilterSpec, SortKey,
};
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
