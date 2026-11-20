//! The read-only state probe of the test build (ADR-0029 §2.6): named, typed getters over the
//! view models, read by `wait` and `expect` steps. It only reads; it has no access to secret
//! fields (the UI never formats a password anyway) and returns no cell value. Text values that
//! can quote a statement or an engine message (`editor.message`) are compared but never written
//! to `results.json` ([`ProbeKey::recordable`]).

use gpui_kit::component::WindowExt as _;
use gpui_kit::{App, Entity, Window};
use ninesql_grid::{FetchState, ResultTab};

use crate::connections::hub::Hub;
use crate::import::flow::Stage;
use crate::workspace::Workspace;

/// The type of a probe value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueKind {
    /// `true` / `false`.
    Bool,
    /// A count.
    Int,
    /// A label.
    Text,
}

impl ValueKind {
    /// "a boolean", "an integer", "a text".
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Bool => "a boolean",
            Self::Int => "an integer",
            Self::Text => "a text",
        }
    }
}

/// A value read by the probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeValue {
    /// A boolean.
    Bool(bool),
    /// A count.
    Int(i64),
    /// A label.
    Text(String),
    /// Nothing to read (no editor, no table, no such connection…).
    Absent,
}

macro_rules! probe_keys {
    ($($variant:ident = $name:literal, $kind:ident, arg = $arg:literal, record = $rec:literal, $doc:literal;)*) => {
        /// Every probe key.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum ProbeKey {
            $(#[doc = $doc] $variant,)*
        }

        impl ProbeKey {
            /// Every key, in documentation order.
            pub const ALL: &[ProbeKey] = &[$(ProbeKey::$variant,)*];

            /// The key's name in scenario files.
            #[must_use]
            pub fn name(self) -> &'static str {
                match self { $(Self::$variant => $name,)* }
            }

            /// The value's type.
            #[must_use]
            pub fn kind(self) -> ValueKind {
                match self { $(Self::$variant => ValueKind::$kind,)* }
            }

            /// Whether the key takes an `arg` (a connection name or a row label).
            #[must_use]
            pub fn takes_arg(self) -> bool {
                match self { $(Self::$variant => $arg,)* }
            }

            /// Whether a text value may be written to `results.json` (labels the UI shows,
            /// never a statement, an engine message or a cell value).
            #[must_use]
            pub fn recordable(self) -> bool {
                match self { $(Self::$variant => $rec,)* }
            }

            /// What the key reads.
            #[must_use]
            pub fn doc(self) -> &'static str {
                match self { $(Self::$variant => $doc,)* }
            }
        }
    };
}

probe_keys! {
    CoreReady = "core.ready", Bool, arg = false, record = true, "The core has started and the connections are loaded.";
    CoreProblems = "core.problems", Int, arg = false, record = true, "Startup and load problems listed by \"Show connection problems\".";
    ConnectionsCount = "connections.count", Int, arg = false, record = true, "Saved connections.";
    ConnectionExists = "connection.exists", Bool, arg = true, record = true, "A saved connection has this name.";
    SessionOpen = "session.open", Bool, arg = true, record = true, "The named connection has an open session.";
    SessionReadOnly = "session.read_only", Bool, arg = true, record = true, "The named connection's session is read-only (engine-side, I3).";
    SessionProduction = "session.production", Bool, arg = true, record = true, "The named connection's session follows the production policy.";
    NavigatorSelected = "navigator.selected", Text, arg = false, record = true, "Label of the selected navigator row.";
    NavigatorRows = "navigator.rows", Int, arg = false, record = true, "Visible navigator rows.";
    NavigatorHasRow = "navigator.has_row", Bool, arg = true, record = true, "A visible navigator row has this label.";
    NavigatorLoading = "navigator.loading", Int, arg = false, record = true, "Navigator loads in flight.";
    DialogOpen = "dialog.open", Bool, arg = false, record = true, "A dialog is open in the window.";
    FocusAny = "focus.any", Bool, arg = false, record = true, "Some element of the window has the keyboard focus.";
    FocusInput = "focus.input", Bool, arg = false, record = true, "The focused element is a text input.";
    TabsCount = "tabs.count", Int, arg = false, record = true, "Open tabs.";
    TabsActiveTitle = "tabs.active_title", Text, arg = false, record = true, "Title of the active tab.";
    EditorConnection = "editor.connection", Text, arg = false, record = true, "Name of the connection the active editor runs on.";
    EditorRunning = "editor.running", Bool, arg = false, record = true, "The active editor is running a statement.";
    EditorAwaitingConfirmation = "editor.awaiting_confirmation", Bool, arg = false, record = true, "The active editor waits for a statement confirmation (\"Run on production?\").";
    EditorMessage = "editor.message", Text, arg = false, record = false, "The active editor's message (compared, never recorded: it can quote an engine message).";
    EditorError = "editor.error", Bool, arg = false, record = true, "The active editor shows an error: its message is an error, or the SQL editor marks a statement's error (F8).";
    EditorCancelLabel = "editor.cancel_label", Text, arg = false, record = true, "\"Cancel\" or \"Stop fetching\" (the driver's cancel capability).";
    GridResultTabs = "grid.result_tabs", Int, arg = false, record = true, "Result tabs of the active editor.";
    GridRows = "grid.rows", Int, arg = false, record = true, "Rows loaded in the first result table of the active editor.";
    GridState = "grid.state", Text, arg = false, record = true, "Fetch state of that table: fetching, idle, complete, truncated, budget, closed, closed-idle, stopped or none.";
    GridFocusedRow = "grid.focused_row", Int, arg = false, record = true, "Row index (0-based, in the result) of the grid's focused cell.";
    GridFocusedColumn = "grid.focused_column", Int, arg = false, record = true, "Column index (0-based, in the result) of the grid's focused cell.";
    GridUpdateCounts = "grid.update_counts", Int, arg = false, record = true, "Statements of the last run that reported an update count.";
    TableOpen = "table.open", Bool, arg = false, record = true, "The active tab shows a table's data (\"Open table data\").";
    TableEditable = "table.editable", Bool, arg = false, record = true, "The table's rows can be edited.";
    TablePendingChanges = "table.pending_changes", Int, arg = false, record = true, "Pending changes of the table's rows.";
    TableSaveConfirmation = "table.save_confirmation", Bool, arg = false, record = true, "The save confirmation is pending.";
    ObjectEditorOpen = "object_editor.open", Bool, arg = false, record = true, "The active tab is an object editor.";
    ObjectEditorTab = "object_editor.tab", Text, arg = false, record = true, "The object editor's active tab (Properties, Columns, Keys / Constraints, Indexes, DDL…).";
    ObjectEditorDdlLoaded = "object_editor.ddl_loaded", Bool, arg = false, record = true, "The object editor's DDL is loaded.";
    ImportStage = "import.stage", Text, arg = false, record = true, "DBeaver import: idle, choose-folder, previewing, preview, applying, vault or done.";
    ImportPreviewConnections = "import.preview_connections", Int, arg = false, record = true, "DBeaver import: connections in the preview.";
    FirstRunOffer = "import.first_run_offer", Bool, arg = false, record = true, "The first-run DBeaver import offer is shown.";
    ExportStage = "export.stage", Text, arg = false, record = true, "The export dialog opened last: none, editing, checking, confirm-overwrite, running, done, failed or cancelled.";
    CsvImportStage = "csv_import.stage", Text, arg = false, record = true, "The CSV import dialog opened last: none, choose-file, previewing, preview, confirm, running, done or failed.";
}

impl ProbeKey {
    /// The key named `name`.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|k| k.name() == name)
    }
}

fn int(n: usize) -> ProbeValue {
    ProbeValue::Int(i64::try_from(n).unwrap_or(i64::MAX))
}

fn fetch_state_name(state: &FetchState) -> &'static str {
    match state {
        FetchState::Fetching { .. } => "fetching",
        FetchState::Idle => "idle",
        FetchState::Complete => "complete",
        FetchState::Truncated(_) => "truncated",
        FetchState::GridBudgetReached => "budget",
        FetchState::Closed => "closed",
        FetchState::ClosedIdle { .. } => "closed-idle",
        FetchState::Stopped => "stopped",
    }
}

/// Reads `key` (with `arg`) from the window of `ws`. Read-only.
pub fn read(
    key: ProbeKey,
    arg: Option<&str>,
    ws: &Entity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) -> ProbeValue {
    use ProbeKey as K;
    use ProbeValue as V;
    if key == K::DialogOpen {
        return V::Bool(window.has_active_dialog(cx));
    }
    if key == K::FocusAny {
        return V::Bool(window.focused(cx).is_some());
    }
    if key == K::FocusInput {
        return V::Bool(window.has_focused_input(cx));
    }
    let cx: &App = cx;
    let hub = Hub::get(cx);
    let connection_id = |name: &str| {
        hub.as_ref().and_then(|h| {
            h.read(cx)
                .connections()
                .iter()
                .find(|c| c.name == name)
                .map(|c| c.id)
        })
    };
    let session_info = |name: &str| {
        let id = connection_id(name)?;
        let h = hub.as_ref()?.read(cx);
        h.sessions(id)
            .first()
            .map(ninesql_core::SessionHandle::info)
    };
    let ws = ws.read(cx);
    let editor = ws.active_editor().map(|e| e.read(cx));
    let first_table = |cx: &App| {
        let e = ws.active_editor()?.read(cx);
        e.results()
            .read(cx)
            .results()
            .tabs()
            .iter()
            .find_map(|t| match t {
                ResultTab::Table(m) => {
                    let m = m.borrow();
                    Some((m.row_count(), fetch_state_name(&m.state())))
                }
                _ => None,
            })
    };
    match key {
        K::CoreReady => V::Bool(hub.as_ref().is_some_and(|h| {
            let h = h.read(cx);
            !h.is_starting() && h.manager().is_some()
        })),
        K::CoreProblems => hub
            .as_ref()
            .map_or(V::Absent, |h| int(h.read(cx).problems().len())),
        K::ConnectionsCount => hub
            .as_ref()
            .map_or(V::Absent, |h| int(h.read(cx).connections().iter().count())),
        K::ConnectionExists => V::Bool(arg.and_then(connection_id).is_some()),
        K::SessionOpen => V::Bool(
            arg.and_then(connection_id)
                .zip(hub.as_ref())
                .is_some_and(|(id, h)| h.read(cx).is_open(id)),
        ),
        K::SessionReadOnly => arg
            .and_then(session_info)
            .map_or(V::Absent, |i| V::Bool(i.read_only)),
        K::SessionProduction => arg
            .and_then(session_info)
            .map_or(V::Absent, |i| V::Bool(i.policy.production)),
        K::NavigatorSelected => {
            let nav = ws.navigator().read(cx);
            nav.selected_index()
                .and_then(|ix| nav.rows(cx).into_iter().nth(ix))
                .map_or(V::Absent, |(_, label)| V::Text(label))
        }
        K::NavigatorRows => int(ws.navigator().read(cx).rows(cx).len()),
        K::NavigatorHasRow => V::Bool(
            ws.navigator()
                .read(cx)
                .rows(cx)
                .iter()
                .any(|(_, l)| Some(l.as_str()) == arg),
        ),
        K::NavigatorLoading => int(ws.navigator().read(cx).loads_in_flight()),
        K::DialogOpen | K::FocusAny | K::FocusInput => V::Absent,
        K::TabsCount => int(ws.tab_titles(cx).len()),
        K::TabsActiveTitle => ws
            .active_title(cx)
            .map_or(V::Absent, |t| V::Text(t.to_string())),
        K::EditorConnection => editor
            .and_then(|e| e.connection())
            .and_then(|id| {
                hub.as_ref()?
                    .read(cx)
                    .connection(id)
                    .map(|c| c.name.clone())
            })
            .map_or(V::Absent, V::Text),
        K::EditorRunning => editor.map_or(V::Absent, |e| V::Bool(e.is_running())),
        K::EditorAwaitingConfirmation => {
            editor.map_or(V::Absent, |e| V::Bool(e.awaiting_confirmation()))
        }
        K::EditorMessage => editor
            .and_then(|e| e.message())
            .map_or(V::Absent, |m| V::Text(m.text.to_string())),
        K::EditorError => editor.map_or(V::Absent, |e| {
            V::Bool(e.message().is_some_and(|m| m.error) || e.editor().read(cx).error().is_some())
        }),
        K::EditorCancelLabel => editor.map_or(V::Absent, |e| V::Text(e.cancel_label().to_owned())),
        K::GridResultTabs => editor.map_or(V::Absent, |e| {
            int(e.results().read(cx).results().tabs().len())
        }),
        K::GridRows => first_table(cx).map_or(V::Absent, |(rows, _)| int(rows)),
        K::GridState => V::Text(first_table(cx).map_or("none", |(_, s)| s).to_owned()),
        K::GridFocusedRow => editor
            .and_then(|e| e.results().read(cx).focused_cell(cx))
            .map_or(V::Absent, |(_, r, _)| int(r)),
        K::GridFocusedColumn => editor
            .and_then(|e| e.results().read(cx).focused_cell(cx))
            .map_or(V::Absent, |(_, _, c)| int(c)),
        K::GridUpdateCounts => editor.map_or(V::Absent, |e| {
            int(e.results().read(cx).results().update_counts().len())
        }),
        K::TableOpen => V::Bool(editor.is_some_and(|e| e.table_state().source.is_some())),
        K::TableEditable => editor.map_or(V::Absent, |e| V::Bool(e.editing().is_some())),
        K::TablePendingChanges => editor
            .and_then(|e| e.editing())
            .map_or(V::Absent, |ed| int(ed.pending.change_count())),
        K::TableSaveConfirmation => {
            editor.map_or(V::Absent, |e| V::Bool(e.confirmation().is_some()))
        }
        K::ObjectEditorOpen => V::Bool(ws.active_object_editor().is_some()),
        K::ObjectEditorTab => ws.active_object_editor().map_or(V::Absent, |o| {
            V::Text(o.read(cx).active().label().to_owned())
        }),
        K::ObjectEditorDdlLoaded => ws
            .active_object_editor()
            .map_or(V::Absent, |o| V::Bool(o.read(cx).ddl_text().is_some())),
        K::ImportStage => V::Text(
            ws.import_flow()
                .map_or("idle", |f| match f.read(cx).stage() {
                    Stage::Idle => "idle",
                    Stage::ChooseFolder => "choose-folder",
                    Stage::Previewing => "previewing",
                    Stage::Preview => "preview",
                    Stage::Applying => "applying",
                    Stage::WaitingForVault(_) => "vault",
                    Stage::Done(_) => "done",
                    #[allow(unreachable_patterns)]
                    _ => "other",
                })
                .to_owned(),
        ),
        K::ImportPreviewConnections => ws
            .import_flow()
            .and_then(|f| f.read(cx).preview().map(|p| p.connections().count()))
            .map_or(V::Absent, int),
        K::FirstRunOffer => V::Bool(ws.first_run_offer(cx).is_some()),
        K::ExportStage => {
            use crate::transfer::export::ExportStage as S;
            let stage = crate::transfer::OPENED_EXPORT.with(|o| {
                o.borrow().as_ref().map(|d| match d.read(cx).stage() {
                    S::Editing => "editing",
                    S::Checking => "checking",
                    S::ConfirmOverwrite(_) => "confirm-overwrite",
                    S::Running { .. } => "running",
                    S::Done(_) => "done",
                    S::Failed(_) => "failed",
                    S::Cancelled => "cancelled",
                })
            });
            V::Text(stage.unwrap_or("none").to_owned())
        }
        K::CsvImportStage => {
            use crate::transfer::import::ImportStage as S;
            let stage = crate::transfer::OPENED_IMPORT.with(|o| {
                o.borrow().as_ref().map(|d| match d.read(cx).stage() {
                    S::ChooseFile => "choose-file",
                    S::Previewing => "previewing",
                    S::Preview => "preview",
                    S::Confirm(_) => "confirm",
                    S::Running { .. } => "running",
                    S::Done(_) => "done",
                    S::Failed(_) => "failed",
                })
            });
            V::Text(stage.unwrap_or("none").to_owned())
        }
    }
}

/// Whether `value` satisfies every matcher.
#[must_use]
pub fn matches(value: &ProbeValue, matchers: &[super::scenario::Matcher]) -> bool {
    use super::scenario::Matcher as M;
    matchers.iter().all(|m| match (m, value) {
        (M::Bool(b), ProbeValue::Bool(v)) => b == v,
        (M::Int(n), ProbeValue::Int(v)) => n == v,
        (M::AtLeast(n), ProbeValue::Int(v)) => v >= n,
        (M::AtMost(n), ProbeValue::Int(v)) => v <= n,
        (M::Text(t), ProbeValue::Text(v)) => t == v,
        (M::Contains(t), ProbeValue::Text(v)) => v.contains(t.as_str()),
        _ => false,
    })
}

/// `value` for `results.json`: booleans and counts as they are, texts only when `key` is
/// [`ProbeKey::recordable`].
#[must_use]
pub fn describe(key: ProbeKey, value: &ProbeValue) -> String {
    match value {
        ProbeValue::Bool(b) => b.to_string(),
        ProbeValue::Int(n) => n.to_string(),
        ProbeValue::Text(t) if key.recordable() => format!("{:?}", crate::browse::display_name(t)),
        ProbeValue::Text(t) => format!("a text of {} characters", t.chars().count()),
        ProbeValue::Absent => "nothing (absent)".to_owned(),
    }
}
