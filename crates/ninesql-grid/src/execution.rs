//! Everything one execution produced (F-SQL-04): one result tab per result set, the update
//! counts, the notices and the statement errors, under one memory budget (I6).
//!
//! # Memory policy
//! - Every [`ResultStream`](ninesql_core::driver_api::ResultStream) already stops at its own
//!   [`FetchBudget::max_bytes`] with `Truncated`. A script runs one stream per statement, so an
//!   execution could hold several budgets' worth of rows: the grid therefore keeps its own
//!   account per execution (the editor tab), [`GridBudget::max_bytes`], over every batch it keeps
//!   ([`batch_bytes`], the measure the stream uses) and every notice.
//! - **Retain, never evict.** Result streams are forward-only: a batch dropped from memory can
//!   only come back by running the statement again, which may not be repeatable (writes, changing
//!   data) and is never done silently. So the grid keeps every batch it accepted and stops
//!   accepting at the budget instead of dropping batches outside a scroll window: a batch that
//!   would cross the budget is not kept, the result is marked
//!   [`FetchState::GridBudgetReached`] (the "Truncated: memory budget reached" banner) and the
//!   stream is released ([`ResultSource::release`](crate::ResultSource::release)) so the server
//!   cursor goes away.
//! - **Everything kept is charged**, not only rows: each result-set header costs
//!   [`RESULT_SET_OVERHEAD_BYTES`] plus its schema ([`schema_bytes`]), each update count
//!   `UPDATE_COUNT_BYTES`, each notice its text plus `NOTICE_OVERHEAD_BYTES`. So a run of many
//!   empty result sets or update counts stays under the same budget as one of many rows.
//! - **At most [`MAX_RESULT_TABS`] result tabs** per execution. A result set past the cap or
//!   past the budget is not kept: it is counted, its stream released, and the
//!   [`ExecutionResults::limit_banner`] says so. An update count past the budget is counted, not
//!   kept (a write's stream is never released: the core only releases reads).
//! - The bytes held are a running total ([`ExecutionResults::bytes`]), not a walk over the tabs.
//! - Nothing else grows with the result: cells are formatted only when drawn or copied (no text
//!   cache), copy is bounded ([`crate::copy::MAX_COPY_BYTES`]), notices are bounded
//!   ([`GridBudget::max_notices`]), document result sets are counted, not kept, until the
//!   document viewer exists (P5-A).
//! - "Fetch all" asks the stream for every remaining row; it ends at the end of the result or
//!   at the first budget reached, so it needs no confirmation: it can never load more than the
//!   budget.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use ninesql_core::driver_api::arrow_schema::{Field, Schema};
use ninesql_core::driver_api::result::{NOTICE_OVERHEAD_BYTES, UPDATE_COUNT_BYTES};
use ninesql_core::driver_api::{
    FetchBudget, NoticeSeverity, ResultItem, ResultSetKind, Truncation, TruncationReason,
};

use crate::model::{FetchState, ResultSetModel, batch_bytes};
use crate::source::{ExecutionEvent, FetchMode, FetchRequest};

/// Most result tabs one execution keeps (I6). Later result sets are counted, not kept, and
/// their streams released.
pub const MAX_RESULT_TABS: usize = 1000;

/// Fixed bytes charged per result-set header: the tab, its model, its column list and, once
/// the tab is shown, its table state. The schema is charged on top ([`schema_bytes`]).
pub const RESULT_SET_OVERHEAD_BYTES: u64 = 1024;

/// Bytes charged per column besides its name and metadata (the field, its column description,
/// its type label).
pub const COLUMN_OVERHEAD_BYTES: u64 = 128;

/// The bytes a result-set schema is charged: per column [`COLUMN_OVERHEAD_BYTES`], its name
/// twice (the schema and the column description each hold it) and its metadata, plus the
/// schema's metadata. Nested fields are counted the same way.
#[must_use]
pub fn schema_bytes(schema: &Schema) -> u64 {
    fn field_bytes(field: &Field) -> u64 {
        use ninesql_core::driver_api::arrow_schema::DataType;
        let own = COLUMN_OVERHEAD_BYTES
            + 2 * field.name().len() as u64
            + field
                .metadata()
                .iter()
                .map(|(k, v)| (k.len() + v.len()) as u64)
                .sum::<u64>();
        let children: u64 = match field.data_type() {
            DataType::Struct(fields) => fields.iter().map(|f| field_bytes(f)).sum(),
            DataType::List(f)
            | DataType::LargeList(f)
            | DataType::FixedSizeList(f, _)
            | DataType::Map(f, _) => field_bytes(f),
            _ => 0,
        };
        own.saturating_add(children)
    }
    schema
        .fields()
        .iter()
        .map(|f| field_bytes(f))
        .fold(0u64, u64::saturating_add)
        .saturating_add(
            schema
                .metadata()
                .iter()
                .map(|(k, v)| (k.len() + v.len()) as u64)
                .sum::<u64>(),
        )
}

/// The bytes a result-set header is charged: [`RESULT_SET_OVERHEAD_BYTES`] plus the schema of
/// a tabular result.
#[must_use]
pub fn header_bytes(kind: &ResultSetKind) -> u64 {
    match kind {
        ResultSetKind::Tabular(schema) => {
            RESULT_SET_OVERHEAD_BYTES.saturating_add(schema_bytes(schema))
        }
        _ => RESULT_SET_OVERHEAD_BYTES,
    }
}

/// A result set shared between the execution model and the table that draws it.
pub type SharedResult = Rc<RefCell<ResultSetModel>>;

/// Limits of one execution's results.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GridBudget {
    /// Bytes of batches and notices kept for the whole execution.
    pub max_bytes: u64,
    /// Rows asked for by "fetch next page" (and expected as the first page).
    pub page_rows: u32,
    /// Notices kept; later ones are counted, not kept.
    pub max_notices: usize,
}

impl GridBudget {
    /// Default number of notices kept.
    pub const DEFAULT_MAX_NOTICES: usize = 10_000;

    /// The grid budget matching the execution's fetch budget: same byte limit, pages of one
    /// server batch.
    #[must_use]
    pub fn from_fetch_budget(budget: &FetchBudget) -> Self {
        Self {
            max_bytes: budget.max_bytes,
            page_rows: budget.effective_batch_rows(),
            max_notices: Self::DEFAULT_MAX_NOTICES,
        }
    }
}

impl Default for GridBudget {
    fn default() -> Self {
        Self::from_fetch_budget(&FetchBudget::default())
    }
}

/// One tab of results.
#[derive(Debug, Clone)]
pub enum ResultTab {
    /// A tabular result set, drawn by the grid.
    Table(SharedResult),
    /// A document result set. Documents are counted and their memory released: the document
    /// viewer comes with P5-A, and the grid does not fake one (I8).
    Documents {
        /// Statement number.
        statement: u32,
        /// Result-set index.
        result_set: u32,
        /// Documents received.
        count: u64,
        /// The stream ended with `Truncated`.
        truncated: Option<TruncationReason>,
    },
}

impl ResultTab {
    /// Statement number.
    #[must_use]
    pub fn statement(&self) -> u32 {
        match self {
            Self::Table(t) => t.borrow().statement(),
            Self::Documents { statement, .. } => *statement,
        }
    }

    /// Tab label: `Result 1`, or `2 · Result 1` for a script's second statement, `(2)` for a
    /// second result set.
    #[must_use]
    pub fn label(&self, multi_statement: bool) -> String {
        let (statement, set) = match self {
            Self::Table(t) => {
                let t = t.borrow();
                (t.statement(), t.result_set())
            }
            Self::Documents {
                statement,
                result_set,
                ..
            } => (*statement, *result_set),
        };
        let mut label = if multi_statement {
            format!("{} · Result", statement + 1)
        } else {
            "Result".to_owned()
        };
        if set > 0 {
            label.push_str(&format!(" ({})", set + 1));
        }
        label
    }
}

/// An update count, for the update counts pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpdateCountEntry {
    /// Statement number.
    pub statement: u32,
    /// Rows affected; `None` when the engine does not report a count.
    pub count: Option<u64>,
}

impl UpdateCountEntry {
    /// Pane text.
    #[must_use]
    pub fn text(&self) -> String {
        match self.count {
            Some(1) => format!("Statement {}: 1 row affected", self.statement + 1),
            Some(n) => format!("Statement {}: {n} rows affected", self.statement + 1),
            None => format!("Statement {}: done", self.statement + 1),
        }
    }
}

/// A server notice, display-only (never logged: it may quote data).
#[derive(Clone, PartialEq, Eq)]
pub struct NoticeEntry {
    /// Statement number.
    pub statement: u32,
    /// Severity.
    pub severity: NoticeSeverity,
    /// Engine code.
    pub code: Option<String>,
    /// Message.
    pub message: String,
}

impl std::fmt::Debug for NoticeEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NoticeEntry")
            .field("statement", &self.statement)
            .field("severity", &self.severity)
            .finish_non_exhaustive()
    }
}

/// Where a statement stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatementStatus {
    /// Running or streaming.
    Running,
    /// The stream ended.
    Finished,
    /// The stream was dropped before its end.
    Closed,
    /// It failed (user-presentable message).
    Failed(String),
    /// The execution was cancelled while it ran.
    Cancelled,
}

/// Where the whole execution stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionState {
    /// Nothing ran yet.
    Idle,
    /// Statements are running or streaming.
    Running,
    /// Every statement ended.
    Finished,
    /// The user cancelled it.
    Cancelled,
}

/// What [`ExecutionResults::apply`] changed, for the view.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Applied {
    /// Everything was cleared (a new execution).
    pub reset: bool,
    /// Tabs added (indices into [`ExecutionResults::tabs`]).
    pub new_tabs: Vec<usize>,
    /// The tab whose rows or state changed.
    pub changed_tab: Option<usize>,
    /// Ask the source to release this statement's stream (grid budget reached).
    pub release: Option<u32>,
}

/// Everything one execution produced. GPUI-free: the view wraps it.
#[derive(Debug, Clone)]
pub struct ExecutionResults {
    budget: GridBudget,
    tabs: Vec<ResultTab>,
    /// Tab index of each statement's current result set.
    current: BTreeMap<u32, usize>,
    statements: BTreeMap<u32, StatementStatus>,
    update_counts: Vec<UpdateCountEntry>,
    notices: Vec<NoticeEntry>,
    notices_dropped: u64,
    /// Running total of the bytes held: headers, kept batches, update counts, notices.
    bytes: u64,
    /// Result sets not kept because [`MAX_RESULT_TABS`] was reached.
    result_sets_over_cap: u64,
    /// Result sets not kept because the byte budget was reached.
    result_sets_over_budget: u64,
    /// Update counts not kept because the byte budget was reached.
    update_counts_dropped: u64,
    /// Statements whose current result set was not kept (their later items are ignored, their
    /// stream was asked to be released once).
    dropped_current: BTreeSet<u32>,
    /// Truncation reported where no tabular result was current (notices, documents).
    other_truncations: Vec<(u32, Truncation)>,
    state: ExecutionState,
}

impl ExecutionResults {
    /// Empty results under `budget`.
    #[must_use]
    pub fn new(budget: GridBudget) -> Self {
        Self {
            budget,
            tabs: Vec::new(),
            current: BTreeMap::new(),
            statements: BTreeMap::new(),
            update_counts: Vec::new(),
            notices: Vec::new(),
            notices_dropped: 0,
            bytes: 0,
            result_sets_over_cap: 0,
            result_sets_over_budget: 0,
            update_counts_dropped: 0,
            dropped_current: BTreeSet::new(),
            other_truncations: Vec::new(),
            state: ExecutionState::Idle,
        }
    }

    /// The budget.
    #[must_use]
    pub fn budget(&self) -> GridBudget {
        self.budget
    }

    /// Result tabs, in arrival order.
    #[must_use]
    pub fn tabs(&self) -> &[ResultTab] {
        &self.tabs
    }

    /// Update counts, in arrival order.
    #[must_use]
    pub fn update_counts(&self) -> &[UpdateCountEntry] {
        &self.update_counts
    }

    /// Notices kept.
    #[must_use]
    pub fn notices(&self) -> &[NoticeEntry] {
        &self.notices
    }

    /// Notices received beyond [`GridBudget::max_notices`] or the byte budget.
    #[must_use]
    pub fn notices_dropped(&self) -> u64 {
        self.notices_dropped
    }

    /// Statement states.
    #[must_use]
    pub fn statements(&self) -> &BTreeMap<u32, StatementStatus> {
        &self.statements
    }

    /// Statement errors, in statement order.
    pub fn errors(&self) -> impl Iterator<Item = (u32, &str)> {
        self.statements.iter().filter_map(|(s, st)| match st {
            StatementStatus::Failed(m) => Some((*s, m.as_str())),
            _ => None,
        })
    }

    /// Truncations that did not land on a table (notices, documents).
    #[must_use]
    pub fn other_truncations(&self) -> &[(u32, Truncation)] {
        &self.other_truncations
    }

    /// Execution state.
    #[must_use]
    pub fn state(&self) -> ExecutionState {
        self.state
    }

    /// More than one statement ran (labels then name the statement).
    #[must_use]
    pub fn multi_statement(&self) -> bool {
        self.statements.len() > 1
    }

    /// Bytes held: result-set headers, kept batches, update counts and notices (a running
    /// total, O(1)).
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Result sets not kept: past [`MAX_RESULT_TABS`] or past the byte budget.
    #[must_use]
    pub fn result_sets_dropped(&self) -> u64 {
        self.result_sets_over_cap + self.result_sets_over_budget
    }

    /// Update counts not kept (byte budget reached).
    #[must_use]
    pub fn update_counts_dropped(&self) -> u64 {
        self.update_counts_dropped
    }

    /// The execution-wide banner when results were not kept (tab cap or budget), else `None`.
    #[must_use]
    pub fn limit_banner(&self) -> Option<String> {
        let mut parts = Vec::new();
        if self.result_sets_over_cap > 0 {
            parts.push(format!(
                "{} more result sets were not kept (a run keeps at most {MAX_RESULT_TABS} result \
                 tabs)",
                self.result_sets_over_cap
            ));
        }
        if self.result_sets_over_budget > 0 {
            parts.push(format!(
                "{} more result sets were not kept (memory budget reached: {} of {})",
                self.result_sets_over_budget,
                format_bytes(self.bytes),
                format_bytes(self.budget.max_bytes)
            ));
        }
        if self.update_counts_dropped > 0 {
            parts.push(format!(
                "{} update counts were not kept (memory budget reached)",
                self.update_counts_dropped
            ));
        }
        (!parts.is_empty()).then(|| {
            format!(
                "Results limit reached: {}. Run fewer statements, or narrow the query, to see the \
                 rest.",
                parts.join("; ")
            )
        })
    }

    /// Applies one event.
    pub fn apply(&mut self, event: ExecutionEvent) -> Applied {
        let mut applied = Applied::default();
        match event {
            ExecutionEvent::Started => {
                *self = Self::new(self.budget);
                self.state = ExecutionState::Running;
                applied.reset = true;
            }
            ExecutionEvent::StatementStarted { statement } => {
                self.state = ExecutionState::Running;
                self.statements.insert(statement, StatementStatus::Running);
            }
            ExecutionEvent::Item { statement, item } => {
                self.statements
                    .entry(statement)
                    .or_insert(StatementStatus::Running);
                self.apply_item(statement, item, &mut applied);
            }
            ExecutionEvent::StatementFinished { statement } => {
                self.end_statement(statement, StatementStatus::Finished, FetchState::Complete);
                applied.changed_tab = self.current.get(&statement).copied();
            }
            ExecutionEvent::StatementClosed { statement } => {
                self.end_statement(statement, StatementStatus::Closed, FetchState::Closed);
                applied.changed_tab = self.current.get(&statement).copied();
            }
            ExecutionEvent::ResultClosedIdle {
                statement, after, ..
            } => {
                // The paused result ends here; the `StatementClosed` that follows keeps this
                // more precise state (a final state is never replaced).
                if let Some(&ix) = self.current.get(&statement) {
                    self.finish_tab(
                        ix,
                        FetchState::ClosedIdle {
                            after_secs: after.as_secs(),
                        },
                    );
                    applied.changed_tab = Some(ix);
                }
            }
            ExecutionEvent::StatementFailed { statement, message } => {
                self.end_statement(
                    statement,
                    StatementStatus::Failed(message),
                    FetchState::Stopped,
                );
                applied.changed_tab = self.current.get(&statement).copied();
            }
            ExecutionEvent::Cancelled => {
                let running: Vec<u32> = self
                    .statements
                    .iter()
                    .filter(|(_, s)| **s == StatementStatus::Running)
                    .map(|(k, _)| *k)
                    .collect();
                for s in running {
                    self.end_statement(s, StatementStatus::Cancelled, FetchState::Stopped);
                }
                self.finish_all(FetchState::Stopped);
                self.state = ExecutionState::Cancelled;
            }
            ExecutionEvent::Finished => {
                // Defensive: a result still open when the execution ends cannot get more rows.
                self.finish_all(FetchState::Closed);
                if self.state != ExecutionState::Cancelled {
                    self.state = ExecutionState::Finished;
                }
            }
        }
        applied
    }

    fn apply_item(&mut self, statement: u32, item: ResultItem, applied: &mut Applied) {
        match item {
            ResultItem::ResultSet(header) => {
                // The statement's previous result set is complete once the next one starts.
                if let Some(&prev) = self.current.get(&statement) {
                    self.finish_tab(prev, FetchState::Complete);
                }
                let cost = header_bytes(&header.kind);
                let refused = if self.tabs.len() >= MAX_RESULT_TABS {
                    self.result_sets_over_cap += 1;
                    true
                } else if self.bytes.saturating_add(cost) > self.budget.max_bytes {
                    self.result_sets_over_budget += 1;
                    true
                } else {
                    false
                };
                if refused {
                    // Not kept: its rows are ignored and its stream released (once).
                    self.current.remove(&statement);
                    if self.dropped_current.insert(statement) {
                        tracing::debug!(statement, "result set not kept; stream released");
                        applied.release = Some(statement);
                    }
                    return;
                }
                self.dropped_current.remove(&statement);
                self.bytes += cost;
                let tab = match header.kind {
                    ResultSetKind::Tabular(schema) => ResultTab::Table(Rc::new(RefCell::new(
                        ResultSetModel::new(statement, header.index, schema, self.budget.page_rows),
                    ))),
                    ResultSetKind::Documents => ResultTab::Documents {
                        statement,
                        result_set: header.index,
                        count: 0,
                        truncated: None,
                    },
                };
                self.tabs.push(tab);
                let ix = self.tabs.len() - 1;
                self.current.insert(statement, ix);
                applied.new_tabs.push(ix);
            }
            ResultItem::Rows(batch) => {
                let Some(&ix) = self.current.get(&statement) else {
                    return;
                };
                let ResultTab::Table(model) = &self.tabs[ix] else {
                    return;
                };
                let model = model.clone();
                applied.changed_tab = Some(ix);
                let size = batch_bytes(&batch);
                if model.borrow().state().is_final() {
                    return;
                }
                if self.bytes.saturating_add(size) > self.budget.max_bytes {
                    model.borrow_mut().finish(FetchState::GridBudgetReached);
                    tracing::debug!(statement, "grid memory budget reached; stream released");
                    applied.release = Some(statement);
                    return;
                }
                let had_rows = model.borrow().row_count() > 0;
                if model.borrow_mut().push_batch(batch).is_err() {
                    // A malformed stream: stop this result, keep what was shown, say why.
                    model.borrow_mut().finish(FetchState::Stopped);
                    self.statements.insert(
                        statement,
                        StatementStatus::Failed(
                            "malformed result: a batch does not match the result's columns"
                                .to_owned(),
                        ),
                    );
                    applied.release = Some(statement);
                } else {
                    self.bytes += size;
                    if !had_rows && model.borrow().row_count() > 0 {
                        // Timing mark for the local measurement protocol (no value, no text).
                        tracing::debug!(statement, "first rows");
                    }
                }
            }
            ResultItem::Documents(docs) => {
                if let Some(&ix) = self.current.get(&statement)
                    && let ResultTab::Documents { count, .. } = &mut self.tabs[ix]
                {
                    *count += docs.len() as u64;
                    applied.changed_tab = Some(ix);
                }
            }
            ResultItem::UpdateCount(count) => {
                if self.bytes.saturating_add(UPDATE_COUNT_BYTES) > self.budget.max_bytes {
                    self.update_counts_dropped += 1;
                } else {
                    self.bytes += UPDATE_COUNT_BYTES;
                    self.update_counts
                        .push(UpdateCountEntry { statement, count });
                }
            }
            ResultItem::Notice(n) => {
                let size = NOTICE_OVERHEAD_BYTES
                    + n.message.len() as u64
                    + n.code.as_ref().map_or(0, |c| c.len() as u64);
                if self.notices.len() >= self.budget.max_notices
                    || self.bytes.saturating_add(size) > self.budget.max_bytes
                {
                    self.notices_dropped += 1;
                } else {
                    self.bytes += size;
                    self.notices.push(NoticeEntry {
                        statement,
                        severity: n.severity,
                        code: n.code,
                        message: n.message,
                    });
                }
            }
            // The end of a result set that was not kept.
            ResultItem::Truncated(_) if self.dropped_current.contains(&statement) => {}
            ResultItem::Truncated(t) => match self.current.get(&statement).copied() {
                Some(ix) => {
                    applied.changed_tab = Some(ix);
                    match &mut self.tabs[ix] {
                        ResultTab::Table(m) => {
                            m.borrow_mut().finish(FetchState::Truncated(t.reason));
                        }
                        ResultTab::Documents { truncated, .. } => {
                            *truncated = Some(t.reason);
                        }
                    }
                }
                None => self.other_truncations.push((statement, t)),
            },
            // `ResultItem` is non-exhaustive: an item kind added later is ignored, never guessed.
            _ => {}
        }
    }

    fn finish_tab(&self, ix: usize, state: FetchState) {
        if let Some(ResultTab::Table(m)) = self.tabs.get(ix) {
            m.borrow_mut().finish(state);
        }
    }

    fn end_statement(&mut self, statement: u32, status: StatementStatus, state: FetchState) {
        for tab in &self.tabs {
            if let ResultTab::Table(m) = tab
                && m.borrow().statement() == statement
            {
                m.borrow_mut().finish(state);
            }
        }
        // An error already recorded (a malformed result) is not hidden by the close that follows.
        if !matches!(
            self.statements.get(&statement),
            Some(StatementStatus::Failed(_))
        ) {
            self.statements.insert(statement, status);
        }
    }

    fn finish_all(&self, state: FetchState) {
        for tab in &self.tabs {
            if let ResultTab::Table(m) = tab {
                m.borrow_mut().finish(state);
            }
        }
    }

    /// Starts a fetch on tab `ix` and returns the request to send, or `None` when that result
    /// cannot fetch now (complete, truncated, closed, already fetching a page…).
    pub fn request_fetch(&mut self, ix: usize, all: bool) -> Option<FetchRequest> {
        let ResultTab::Table(model) = self.tabs.get(ix)? else {
            return None;
        };
        let mode = if all {
            FetchMode::All
        } else {
            FetchMode::Page {
                rows: self.budget.page_rows,
            }
        };
        let mut m = model.borrow_mut();
        m.begin_fetch(mode).then(|| FetchRequest {
            statement: m.statement(),
            result_set: m.result_set(),
            mode,
        })
    }
}

/// Human-readable size (`12.3 MiB`).
#[must_use]
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut v = bytes as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use ninesql_core::driver_api::{Document, Notice, ResultSetHeader};

    use super::*;
    use crate::model::tests::{batch, schema};

    fn header(index: u32) -> ResultItem {
        ResultItem::ResultSet(ResultSetHeader::tabular(index, schema()))
    }

    fn item(statement: u32, item: ResultItem) -> ExecutionEvent {
        ExecutionEvent::Item { statement, item }
    }

    fn table(r: &ExecutionResults, ix: usize) -> SharedResult {
        match &r.tabs()[ix] {
            ResultTab::Table(t) => t.clone(),
            ResultTab::Documents { .. } => panic!("not a table"),
        }
    }

    #[test]
    fn script_with_several_results_counts_and_notices() {
        let mut r = ExecutionResults::new(GridBudget::default());
        assert!(r.apply(ExecutionEvent::Started).reset);
        r.apply(ExecutionEvent::StatementStarted { statement: 0 });
        assert_eq!(r.apply(item(0, header(0))).new_tabs, vec![0]);
        r.apply(item(0, ResultItem::Rows(batch(0, 10))));
        r.apply(item(0, header(1)));
        r.apply(item(0, ResultItem::Rows(batch(0, 3))));
        r.apply(ExecutionEvent::StatementFinished { statement: 0 });
        r.apply(ExecutionEvent::StatementStarted { statement: 1 });
        r.apply(item(1, ResultItem::UpdateCount(Some(5))));
        r.apply(item(
            1,
            ResultItem::Notice(Notice::new(NoticeSeverity::Warning, "careful").with_code("01000")),
        ));
        r.apply(ExecutionEvent::StatementFinished { statement: 1 });
        r.apply(ExecutionEvent::StatementStarted { statement: 2 });
        r.apply(ExecutionEvent::StatementFailed {
            statement: 2,
            message: "syntax error at or near \"FORM\"".into(),
        });
        r.apply(ExecutionEvent::Finished);

        assert_eq!(r.tabs().len(), 2);
        assert_eq!(table(&r, 0).borrow().state(), FetchState::Complete);
        assert_eq!(table(&r, 1).borrow().row_count(), 3);
        assert!(r.multi_statement());
        assert_eq!(r.tabs()[0].label(true), "1 · Result");
        assert_eq!(r.tabs()[1].label(true), "1 · Result (2)");
        assert_eq!(r.tabs()[1].label(false), "Result (2)");
        assert_eq!(r.update_counts()[0].text(), "Statement 2: 5 rows affected");
        assert_eq!(r.notices()[0].code.as_deref(), Some("01000"));
        assert_eq!(r.errors().count(), 1);
        assert_eq!(r.state(), ExecutionState::Finished);
        assert!(!format!("{:?}", r.notices()[0]).contains("careful"));
        // A new execution clears everything.
        r.apply(ExecutionEvent::Started);
        assert!(r.tabs().is_empty() && r.notices().is_empty() && r.update_counts().is_empty());
    }

    #[test]
    fn paging_requests() {
        let budget = GridBudget {
            page_rows: 10,
            ..GridBudget::default()
        };
        let mut r = ExecutionResults::new(budget);
        r.apply(ExecutionEvent::Started);
        r.apply(item(0, header(0)));
        assert_eq!(r.request_fetch(0, false), None, "first page in flight");
        r.apply(item(0, ResultItem::Rows(batch(0, 10))));
        let req = r.request_fetch(0, false).unwrap();
        assert_eq!(
            req,
            FetchRequest {
                statement: 0,
                result_set: 0,
                mode: FetchMode::Page { rows: 10 }
            }
        );
        assert_eq!(r.request_fetch(0, false), None, "one page at a time");
        assert_eq!(r.request_fetch(0, true).unwrap().mode, FetchMode::All);
        r.apply(item(0, ResultItem::Rows(batch(10, 10))));
        r.apply(item(0, ResultItem::Rows(batch(20, 10))));
        assert!(table(&r, 0).borrow().state().is_fetching());
        r.apply(item(
            0,
            ResultItem::Truncated(Truncation::new(TruncationReason::MaxBytes, 30, 9000)),
        ));
        r.apply(ExecutionEvent::StatementFinished { statement: 0 });
        assert_eq!(
            table(&r, 0).borrow().state(),
            FetchState::Truncated(TruncationReason::MaxBytes)
        );
        assert_eq!(r.request_fetch(0, true), None);
        assert_eq!(r.request_fetch(5, true), None);
    }

    #[test]
    fn grid_budget_across_statements_releases_the_stream() {
        let one = batch_bytes(&batch(0, 100));
        let headers = 2 * header_bytes(&ResultSetKind::Tabular(schema()));
        let budget = GridBudget {
            max_bytes: one * 3 + one / 2 + headers,
            page_rows: 100,
            max_notices: 10,
        };
        let mut r = ExecutionResults::new(budget);
        r.apply(ExecutionEvent::Started);
        r.apply(item(0, header(0)));
        r.apply(item(0, ResultItem::Rows(batch(0, 100))));
        r.apply(item(0, ResultItem::Rows(batch(0, 100))));
        r.apply(ExecutionEvent::StatementFinished { statement: 0 });
        r.apply(item(1, header(0)));
        assert_eq!(
            r.apply(item(1, ResultItem::Rows(batch(0, 100)))).release,
            None
        );
        let applied = r.apply(item(1, ResultItem::Rows(batch(0, 100))));
        assert_eq!(applied.release, Some(1));
        assert!(r.bytes() <= budget.max_bytes);
        let t = table(&r, 1);
        assert_eq!(t.borrow().state(), FetchState::GridBudgetReached);
        assert!(t.borrow().state().budget_reached());
        assert_eq!(t.borrow().row_count(), 100);
        // Later batches (in flight before the release) are ignored.
        assert_eq!(
            r.apply(item(1, ResultItem::Rows(batch(0, 1)))).release,
            None
        );
        r.apply(ExecutionEvent::StatementClosed { statement: 1 });
        assert_eq!(t.borrow().state(), FetchState::GridBudgetReached);
    }

    #[test]
    fn notices_are_bounded() {
        let budget = GridBudget {
            max_notices: 2,
            ..GridBudget::default()
        };
        let mut r = ExecutionResults::new(budget);
        for _ in 0..5 {
            r.apply(item(
                0,
                ResultItem::Notice(Notice::new(NoticeSeverity::Info, "x")),
            ));
        }
        assert_eq!(r.notices().len(), 2);
        assert_eq!(r.notices_dropped(), 3);
        assert!(r.bytes() > 0);
    }

    #[test]
    fn documents_are_counted_not_kept_and_cancel_stops_results() {
        let mut r = ExecutionResults::new(GridBudget::default());
        r.apply(ExecutionEvent::Started);
        r.apply(item(
            0,
            ResultItem::ResultSet(ResultSetHeader::documents(0)),
        ));
        r.apply(item(
            0,
            ResultItem::Documents(vec![Document::new(vec![]), Document::new(vec![])]),
        ));
        assert!(matches!(r.tabs()[0], ResultTab::Documents { count: 2, .. }));
        r.apply(ExecutionEvent::StatementStarted { statement: 1 });
        r.apply(item(1, header(0)));
        r.apply(ExecutionEvent::Cancelled);
        assert_eq!(table(&r, 1).borrow().state(), FetchState::Stopped);
        assert_eq!(r.state(), ExecutionState::Cancelled);
        assert_eq!(r.statements()[&1], StatementStatus::Cancelled);
        r.apply(ExecutionEvent::Finished);
        assert_eq!(r.state(), ExecutionState::Cancelled);
    }

    #[test]
    fn a_malformed_batch_stops_the_result_with_an_error() {
        use std::sync::Arc;

        use ninesql_core::driver_api::arrow_array::{Int64Array, RecordBatch};
        use ninesql_core::driver_api::arrow_schema::{DataType, Field, Schema};

        let mut r = ExecutionResults::new(GridBudget::default());
        r.apply(item(0, header(0)));
        r.apply(item(0, ResultItem::Rows(batch(0, 2))));
        let one = RecordBatch::try_new(
            Arc::new(Schema::new(vec![Field::new("x", DataType::Int64, false)])),
            vec![Arc::new(Int64Array::from(vec![1]))],
        )
        .unwrap();
        assert_eq!(r.apply(item(0, ResultItem::Rows(one))).release, Some(0));
        r.apply(ExecutionEvent::StatementClosed { statement: 0 });
        assert_eq!(table(&r, 0).borrow().state(), FetchState::Stopped);
        assert_eq!(table(&r, 0).borrow().row_count(), 2);
        assert!(r.errors().next().unwrap().1.starts_with("malformed result"));
    }

    #[test]
    fn truncation_without_a_table_is_kept() {
        let mut r = ExecutionResults::new(GridBudget::default());
        r.apply(item(
            0,
            ResultItem::Truncated(Truncation::new(TruncationReason::MaxRows, 0, 10)),
        ));
        assert_eq!(r.other_truncations().len(), 1);
    }

    /// What `bytes()` must equal: a walk over everything kept.
    fn recount(r: &ExecutionResults) -> u64 {
        let tabs: u64 = r
            .tabs()
            .iter()
            .map(|t| match t {
                ResultTab::Table(m) => {
                    let m = m.borrow();
                    header_bytes(&ResultSetKind::Tabular(m.schema().clone())) + m.bytes()
                }
                ResultTab::Documents { .. } => RESULT_SET_OVERHEAD_BYTES,
            })
            .sum();
        let notices: u64 = r
            .notices()
            .iter()
            .map(|n| {
                NOTICE_OVERHEAD_BYTES
                    + n.message.len() as u64
                    + n.code.as_ref().map_or(0, |c| c.len() as u64)
            })
            .sum();
        tabs + notices + UPDATE_COUNT_BYTES * r.update_counts().len() as u64
    }

    #[test]
    fn many_empty_result_sets_stay_bounded_and_show_the_banner() {
        let mut r = ExecutionResults::new(GridBudget::default());
        r.apply(ExecutionEvent::Started);
        r.apply(ExecutionEvent::StatementStarted { statement: 0 });
        let mut releases = Vec::new();
        for i in 0..100_000u32 {
            if let Some(s) = r.apply(item(0, header(i))).release {
                releases.push(s);
            }
            // Rows and the truncation of a result set not kept are ignored.
            r.apply(item(0, ResultItem::Rows(batch(0, 0))));
        }
        r.apply(ExecutionEvent::StatementFinished { statement: 0 });
        assert_eq!(r.tabs().len(), MAX_RESULT_TABS);
        assert_eq!(r.result_sets_dropped(), 100_000 - MAX_RESULT_TABS as u64);
        assert_eq!(releases, vec![0], "the stream is released once");
        assert!(r.bytes() <= r.budget().max_bytes);
        assert_eq!(r.bytes(), recount(&r));
        let banner = r.limit_banner().unwrap();
        assert!(banner.starts_with("Results limit reached: 99000 more result sets were not kept"));
        assert!(banner.contains("at most 1000 result tabs"));

        // The same under a small budget: headers alone reach it.
        let cost = header_bytes(&ResultSetKind::Tabular(schema()));
        let budget = GridBudget {
            max_bytes: cost * 10,
            ..GridBudget::default()
        };
        let mut r = ExecutionResults::new(budget);
        r.apply(ExecutionEvent::Started);
        let mut releases = 0;
        for s in 0..100_000u32 {
            r.apply(ExecutionEvent::StatementStarted { statement: s });
            if r.apply(item(s, header(0))).release.is_some() {
                releases += 1;
            }
            r.apply(item(
                s,
                ResultItem::Truncated(Truncation::new(TruncationReason::MaxRows, 0, 0)),
            ));
            r.apply(ExecutionEvent::StatementClosed { statement: s });
        }
        assert_eq!(r.tabs().len(), 10);
        assert_eq!(releases, 100_000 - 10);
        assert!(r.other_truncations().is_empty());
        assert!(r.bytes() <= budget.max_bytes);
        assert!(r.limit_banner().unwrap().contains("memory budget reached"));
    }

    #[test]
    fn update_counts_are_charged() {
        let budget = GridBudget {
            max_bytes: UPDATE_COUNT_BYTES * 3,
            ..GridBudget::default()
        };
        let mut r = ExecutionResults::new(budget);
        r.apply(ExecutionEvent::Started);
        for s in 0..10 {
            assert_eq!(
                r.apply(item(s, ResultItem::UpdateCount(Some(1)))).release,
                None,
                "a write is never released"
            );
        }
        assert_eq!(r.update_counts().len(), 3);
        assert_eq!(r.update_counts_dropped(), 7);
        assert_eq!(r.bytes(), UPDATE_COUNT_BYTES * 3);
        assert_eq!(
            r.limit_banner().unwrap(),
            "Results limit reached: 7 update counts were not kept (memory budget reached). Run \
             fewer statements, or narrow the query, to see the rest."
        );
        r.apply(ExecutionEvent::Started);
        assert_eq!(r.bytes(), 0);
        assert!(r.limit_banner().is_none());
    }

    #[test]
    fn the_running_total_matches_what_is_kept() {
        let mut r = ExecutionResults::new(GridBudget::default());
        r.apply(ExecutionEvent::Started);
        r.apply(item(0, header(0)));
        assert_eq!(r.bytes(), header_bytes(&ResultSetKind::Tabular(schema())));
        r.apply(item(0, ResultItem::Rows(batch(0, 10))));
        r.apply(item(0, ResultItem::Rows(batch(10, 0))));
        r.apply(item(0, header(1)));
        r.apply(item(0, ResultItem::Rows(batch(0, 3))));
        r.apply(item(1, ResultItem::UpdateCount(None)));
        r.apply(item(
            1,
            ResultItem::Notice(Notice::new(NoticeSeverity::Info, "hello").with_code("00000")),
        ));
        r.apply(item(
            2,
            ResultItem::ResultSet(ResultSetHeader::documents(0)),
        ));
        r.apply(item(2, ResultItem::Documents(vec![Document::new(vec![])])));
        assert_eq!(r.bytes(), recount(&r));
        assert!(r.limit_banner().is_none());
    }

    #[test]
    fn an_idle_close_keeps_the_rows_and_stops_fetching() {
        let budget = GridBudget {
            page_rows: 10,
            ..GridBudget::default()
        };
        let mut r = ExecutionResults::new(budget);
        r.apply(ExecutionEvent::Started);
        r.apply(item(0, header(0)));
        r.apply(item(0, ResultItem::Rows(batch(0, 10))));
        assert_eq!(table(&r, 0).borrow().state(), FetchState::Idle);
        let applied = r.apply(ExecutionEvent::ResultClosedIdle {
            statement: 0,
            rows: 10,
            after: std::time::Duration::from_secs(240),
        });
        assert_eq!(applied.changed_tab, Some(0));
        r.apply(ExecutionEvent::StatementClosed { statement: 0 });
        r.apply(ExecutionEvent::Finished);
        assert_eq!(
            table(&r, 0).borrow().state(),
            FetchState::ClosedIdle { after_secs: 240 }
        );
        assert_eq!(table(&r, 0).borrow().row_count(), 10);
        assert_eq!(r.request_fetch(0, false), None);
        assert_eq!(r.request_fetch(0, true), None);
        assert_eq!(r.statements()[&0], StatementStatus::Closed);
    }

    #[test]
    fn sizes_are_human_readable() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(256 * 1024 * 1024), "256.0 MiB");
        assert_eq!(format_bytes(1536), "1.5 KiB");
    }
}
