//! The seams between the SQL editor, the results grid and the core's execution service
//! (P3-C): the editor's [`ExecutionSink`], the grid's [`ResultSource`] over a [`RunControl`],
//! the editor request turned into a [`RunRequest`], and the user-facing text of core errors.
//!
//! Nothing here does I/O or waits: `submit`, `cancel`, `fetch` and `release` only queue work
//! (I6). The run itself is polled on GPUI's background executor by the tab
//! ([`super::SqlEditorTab`]), which applies each event on the foreground thread.

use std::cell::RefCell;
use std::rc::Rc;

use gpui_kit::base::async_util::Sender;
use ninesql_core::driver_api::{DriverError, FetchBudget};
use ninesql_core::{CoreError, ErrorPosition, FetchMode, RunControl, RunRequest};
use ninesql_editor::execution::{
    ErrorLocation, ExecutionItem, ExecutionRequest, ExecutionSink, RequestId,
};
use ninesql_editor::split::ItemKind;
use ninesql_grid::{FetchRequest, ResultSource};

/// The run a tab is driving: what the sink cancels and the grid's source fetches from.
#[derive(Clone, Debug)]
pub struct CurrentRun {
    /// The editor request it answers.
    pub request: RequestId,
    /// Paging, release and cancel of the core run.
    pub control: RunControl,
    /// Grid statement number of the run's statement 0 (a run resumed after a confirmation
    /// continues the numbering of the run it resumes).
    pub base: u32,
}

/// Shared between the tab, its sink and its result source.
pub type SharedRun = Rc<RefCell<Option<CurrentRun>>>;

/// The editor's [`ExecutionSink`]: requests go to the tab through a channel (the tab reads them
/// on the foreground executor, outside the editor's update), cancels go straight to the core's
/// [`RunControl`].
pub struct TabSink {
    requests: Sender<ExecutionRequest>,
    current: SharedRun,
}

impl TabSink {
    /// A sink sending requests on `requests`, cancelling the run in `current`.
    #[must_use]
    pub fn new(requests: Sender<ExecutionRequest>, current: SharedRun) -> Self {
        Self { requests, current }
    }
}

impl ExecutionSink for TabSink {
    fn submit(&self, request: ExecutionRequest) {
        if self.requests.try_send(request).is_err() {
            tracing::debug!("editor request dropped: the tab is gone");
        }
    }

    fn cancel(&self, id: RequestId) {
        if let Some(run) = self.current.borrow().as_ref()
            && run.request == id
        {
            run.control.cancel();
        }
    }
}

/// The grid's [`ResultSource`] over the current run's [`RunControl`]. Requests for a
/// statement of an earlier run (before a confirmation) are ignored: their stream is gone.
pub struct RunSource {
    current: SharedRun,
}

impl RunSource {
    /// A source over the run in `current`.
    #[must_use]
    pub fn new(current: SharedRun) -> Self {
        Self { current }
    }

    fn index(&self, statement: u32) -> Option<(RunControl, usize)> {
        let current = self.current.borrow();
        let run = current.as_ref()?;
        let index = statement.checked_sub(run.base)?;
        Some((run.control.clone(), index as usize))
    }
}

impl ResultSource for RunSource {
    fn fetch(&self, request: FetchRequest) {
        let Some((control, index)) = self.index(request.statement) else {
            return;
        };
        let mode = match request.mode {
            ninesql_grid::FetchMode::Page { rows } => FetchMode::Page(rows),
            ninesql_grid::FetchMode::All => FetchMode::All,
        };
        control.fetch(index, mode);
    }

    fn release(&self, statement: u32) {
        if let Some((control, index)) = self.index(statement) {
            control.release(index);
        }
    }
}

/// Why an editor request cannot become a run (shown under the editor).
pub type BuildError = String;

/// The core request for `request`: a single item runs as one statement with its bind
/// parameters (rewritten text, offset of the item in the buffer); several items run as the
/// selection of the buffer from the first to the last one, which the core splits again with the
/// session's lexical modes (so a mis-split never runs something else than what was classified).
///
/// # Errors
/// Nothing to run, a refused item, bind parameters in a script, or a buffer edited since the
/// request.
pub fn build_run_request(
    request: &ExecutionRequest,
    document: &str,
    budget: FetchBudget,
    page_rows: u32,
) -> Result<RunRequest, BuildError> {
    let run = match request.items.as_slice() {
        [] => return Err("Nothing to run here.".to_owned()),
        [item] => {
            refuse_item(item)?;
            let mut run =
                RunRequest::statement(item.text.clone()).with_params(item.params.clone());
            run.offset = item.span.start;
            run
        }
        items => {
            if let Some(item) = items.iter().find(|i| matches!(i.kind, ItemKind::Refused(_))) {
                refuse_item(item)?;
            }
            if items.iter().any(|i| !i.params.is_empty()) {
                return Err(
                    "Bind parameters need a single statement: run the statements one at a time."
                        .to_owned(),
                );
            }
            if items
                .iter()
                .any(|i| document.get(i.span.clone()) != Some(i.original.as_str()))
            {
                return Err("The script changed since the run was asked: run it again.".to_owned());
            }
            let (Some(first), Some(last)) = (items.first(), items.last()) else {
                return Err("Nothing to run here.".to_owned());
            };
            let Some(text) = document.get(first.span.start..last.span.end) else {
                return Err("The script changed since the run was asked: run it again.".to_owned());
            };
            RunRequest::selection(text.to_owned(), first.span.start)
        }
    };
    Ok(run.with_budget(budget).with_paging(page_rows))
}

fn refuse_item(item: &ExecutionItem) -> Result<(), BuildError> {
    match &item.kind {
        ItemKind::Statement => Ok(()),
        ItemKind::ClientDirective => {
            Err("This is a client directive, not a statement: nothing is sent.".to_owned())
        }
        ItemKind::Refused(why) => Err(format!(
            "Line {}: {why}. Nothing was run.",
            item.start.line + 1
        )),
    }
}

/// The editor item an error of the core belongs to, and where in it: the item containing the
/// error position or, without one, the statement start. `None` when no item matches (the core
/// split the text differently): the message is then shown without a position.
#[must_use]
pub fn locate_error(
    request: &ExecutionRequest,
    statement_start: Option<usize>,
    position: Option<&ErrorPosition>,
) -> (usize, Option<ErrorLocation>) {
    let contains = |item: &ExecutionItem, offset: usize| {
        item.span.start <= offset && offset <= item.span.end.max(item.span.start + item.text.len())
    };
    if let Some(pos) = position
        && let Some((ix, item)) = request
            .items
            .iter()
            .enumerate()
            .find(|(_, i)| contains(i, pos.offset))
    {
        let local = pos.offset - item.span.start;
        let chars = item
            .text
            .get(..local.min(item.text.len()))
            .map_or(0, |t| t.chars().count());
        let n = u32::try_from(chars + 1).unwrap_or(u32::MAX);
        return (ix, Some(ErrorLocation::CharOffset(n)));
    }
    if let Some(start) = statement_start
        && let Some(ix) = request.items.iter().position(|i| contains(i, start))
    {
        return (ix, None);
    }
    if request.items.len() == 1 {
        return (0, None);
    }
    (usize::MAX, None)
}

/// The user-facing text of a core error (never a secret or a value: core and driver messages
/// are display-only).
#[must_use]
pub fn core_error_text(error: &CoreError) -> String {
    match error {
        CoreError::PendingTransaction => "A transaction is open on this session: commit or roll back first (Commit transaction / Roll back transaction).".to_owned(),
        CoreError::InvalidRequest(why) => format!("Not run: {why}."),
        CoreError::Driver(DriverError::Busy) => "The session is busy with another statement or a paused result: cancel it, or fetch the result to its end, then try again.".to_owned(),
        CoreError::SessionClosed => "The session is closed: connect again and choose the connection for this editor.".to_owned(),
        other => other.to_string(),
    }
}

/// The engine's error code, when it gave one.
#[must_use]
pub fn error_code(error: &CoreError) -> Option<String> {
    match error {
        CoreError::Driver(DriverError::Query { code, .. }) => code.clone(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ninesql_core::driver_api::AnsiDialect;
    use ninesql_editor::execution::{ExecutionKind, ExecutionPlan};
    use ninesql_editor::params::ParameterStyle;
    use ninesql_editor::split::DialectSplitter;
    use std::sync::Arc;

    fn request(doc: &str, kind: ExecutionKind, cursor: usize) -> ExecutionRequest {
        let splitter = DialectSplitter::new(Arc::new(AnsiDialect));
        let plan = ExecutionPlan::build(doc, 0..0, cursor, kind, &splitter, ParameterStyle::ANSI)
            .unwrap_or_else(|e| panic!("{e}"));
        plan.into_request(RequestId(1), None, None, 0)
    }

    #[test]
    fn one_item_runs_as_a_statement_at_its_offset() {
        let doc = "select 1;\nselect 2;";
        let r = request(doc, ExecutionKind::Statement, 12);
        let run = build_run_request(&r, doc, FetchBudget::default(), 100)
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(run.text, "select 2");
        assert_eq!(run.offset, 10);
        assert_eq!(run.kind, ninesql_core::RunKind::Statement);
        assert_eq!(run.page_rows, Some(100));
    }

    #[test]
    fn several_items_run_as_the_buffer_selection() {
        let doc = "  select 1; -- c\nselect 2;  ";
        let r = request(doc, ExecutionKind::Script, 0);
        let run = build_run_request(&r, doc, FetchBudget::default(), 100)
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(run.text, "select 1; -- c\nselect 2");
        assert_eq!(run.offset, 2);
        assert_eq!(run.kind, ninesql_core::RunKind::Script);
        // An edited buffer refuses the run instead of running other text.
        let edited = "  select 9; -- c\nselect 2;  ";
        assert!(build_run_request(&r, edited, FetchBudget::default(), 100).is_err());
    }

    #[test]
    fn errors_land_on_their_item() {
        let doc = "select 1;\nselect bad;";
        let r = request(doc, ExecutionKind::Script, 0);
        let pos = ErrorPosition {
            offset: 17,
            line: 2,
            column: 8,
        };
        assert_eq!(
            locate_error(&r, Some(10), Some(&pos)),
            (1, Some(ErrorLocation::CharOffset(8)))
        );
        assert_eq!(locate_error(&r, Some(10), None), (1, None));
        assert_eq!(locate_error(&r, None, None), (usize::MAX, None));
    }

    #[test]
    fn core_errors_read_well() {
        assert!(core_error_text(&CoreError::PendingTransaction).contains("commit or roll back"));
        assert!(core_error_text(&CoreError::InvalidRequest("x")).starts_with("Not run: x"));
        assert!(core_error_text(&CoreError::Driver(DriverError::Busy)).contains("busy"));
    }
}
