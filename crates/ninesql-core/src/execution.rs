//! Query execution service (P3-C): runs a statement, a selection or a script on a session, one
//! statement at a time, and streams per-statement events to the UI (F-SQL-03, F-SQL-04,
//! F-SQL-05, F-SQL-07, F-SQL-12). It sits on top of [`SessionHandle::execute`], so every rule of
//! the session manager still applies to each statement: classification and the production /
//! destructive confirmation (ADR-0007), the bounded result channel and the memory budget (I6),
//! the cancel path and `Busy` handling after a cancel (M6), query text logged only at `trace`
//! level and never when the type forbids it.
//!
//! # Using it (P3-A editor, P3-B grid)
//! ```text
//! let run = core.execution().run(&session, RunRequest::script(buffer_text));
//! // or RunRequest::statement(text) / RunRequest::selection(selected_text, selection_start)
//! loop { match run.next().await { … RunEvent::Done(summary) => break } }
//! run.cancel();   // "Stop": cancels the running statement server-side, then the script ends
//! ```
//! - **Splitting.** A script or selection is split with the engine's dialect
//!   ([`ninesql_driver_api::Dialect::split_statements`], registry dialect of the session's
//!   engine). The driver re-checks each statement with its own session lexical modes and refuses
//!   text that still holds several statements ([`DriverError::MultipleStatements`]), so a
//!   mis-split never runs something other than what was classified.
//! - **Events.** A run starts with [`RunEvent::Started`]. Each statement (identified by its
//!   0-based index in the text) reports [`RunEvent::StatementStarted`], its driver result items
//!   unchanged ([`RunEvent::Item`]: result-set headers, row batches, documents, update counts,
//!   notices, `Truncated`) and exactly one statement end: [`RunEvent::StatementFinished`],
//!   [`RunEvent::StatementClosed`] (released early), [`RunEvent::StatementFailed`] (with an
//!   [`ErrorPosition`] when the engine reports one), [`RunEvent::StatementCancelled`],
//!   [`RunEvent::StatementTimedOut`] or [`RunEvent::NeedsConfirmation`]. The run ends with
//!   exactly one [`RunEvent::Done`], which is never dropped.
//! - **Paging (P3-B grid).** With [`RunRequest::with_paging`], the first page of every result
//!   set is delivered without being asked, then the result pauses ([`RunEvent::PageEnd`]) with
//!   its stream open until [`RunControl::fetch`] (a page, or all of it under the budget) or
//!   [`RunControl::release`] (closed early: [`RunEvent::StatementClosed`]). The grid's result
//!   source maps `fetch` / `release` onto a [`RunControl`]; the core does not depend on the
//!   grid crate.
//! - **Back-pressure.** Events go through a small bounded channel: a UI that reads slowly slows
//!   the fetch down (on top of the result's own budget). After a cancel, result items still
//!   queued are dropped; the statement end and `Done` still arrive.
//! - **Confirmation.** A statement that needs confirmation is not sent: the run ends with
//!   [`RunEnd::NeedsConfirmation`]. The UI shows the dialog and calls `run` again with the same
//!   text, [`RunRequest::resume_from`] the statement's start offset
//!   ([`RunEvent::NeedsConfirmation::span`]) and the token ([`RunRequest::confirmed`]):
//!   statements before it are not run again. Resuming is by byte offset, never by index:
//!   indexes depend on the lexical modes in force when the text is split (review N1).
//! - **Timeouts.** [`TimeoutChoice::Default`] uses the connection's default from the
//!   preferences, else the global default (none unless set, ADR-0004); a request may ask for
//!   another one or none. The timeout covers each statement, fetch included.
//! - **Transactions.** After each statement the session's [`TransactionState`] is reported
//!   ([`RunEvent::StatementFinished::transaction`], [`RunSummary::transaction`]); the session
//!   also emits [`crate::CoreEvent::TransactionChanged`]. Auto-commit versus manual follows the
//!   connection's `auto_commit` and its type at open ([`crate::Connection::policy`]).
//! - **History.** Each finished statement is handed to the [`History`] under its rule
//!   ([`crate::history`]): never parameter values, nothing for production or for types that do
//!   not allow query text in logs. Statements sent through [`SessionHandle::execute`] directly
//!   are not recorded.

use std::fmt;
use std::ops::Range;
use std::sync::{Arc, PoisonError, RwLock};
use std::time::{Duration, Instant, SystemTime};

use ninesql_driver_api::{
    CancelToken, DriverError, ExecutionId, FetchBudget, Param, QueryLanguage, ResultItem,
    StatementClass, StatementSpan,
};
use ninesql_storage::history::EntryStatus;
use tokio::sync::mpsc;

use crate::connection::EngineId;
use crate::error::CoreError;
use crate::history::{History, StatementRecord};
use crate::preferences::{OnError, Preferences};
use crate::session::{
    ConfirmationRequest, ConfirmationToken, ExecEvent, ExecuteRequest, Next, SessionHandle,
    SessionManager, TransactionState,
};

/// Events buffered per run before the fetch waits for the UI (plus one slot reserved for
/// [`RunEvent::Done`]).
const RUN_QUEUE: usize = 4;

/// What the request holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RunKind {
    /// Exactly one statement ("Execute statement"); text that splits into several fails with
    /// [`CoreError::InvalidRequest`] before anything is sent.
    Statement,
    /// Every statement of the text, in order ("Execute script", or a selection).
    Script,
}

/// Which timeout a run uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum TimeoutChoice {
    /// The connection's default timeout from the preferences, else the global default.
    #[default]
    Default,
    /// No timeout.
    None,
    /// This timeout, for each statement.
    After(Duration),
}

/// A run of one statement or a script. `Debug` shows the text length only.
#[derive(Clone)]
#[non_exhaustive]
pub struct RunRequest {
    /// Text to run (a statement, a selection or a whole script).
    pub text: String,
    /// Byte offset of `text` in the editor buffer (for a selection): added to every span and
    /// error offset reported, so they point into the buffer.
    pub offset: usize,
    /// Statement or script.
    pub kind: RunKind,
    /// Language.
    pub language: QueryLanguage,
    /// Bind parameters (F-SQL-06). Only with a single statement. Never logged or recorded.
    pub params: Vec<Param>,
    /// Timeout.
    pub timeout: TimeoutChoice,
    /// Memory budget of each result; `None`: from the preferences.
    pub budget: Option<FetchBudget>,
    /// What to do after a failed statement; `None`: from the preferences.
    pub on_error: Option<OnError>,
    /// Byte offset in the editor buffer ([`Self::offset`] included) where the run starts:
    /// only `text` from there on is split and run (resuming after a confirmation).
    pub resume_from: Option<usize>,
    /// Confirmation of the first statement of the run; used only when that statement starts
    /// exactly at [`Self::resume_from`] (or, without it, for the first statement of the text).
    pub confirmation: Option<ConfirmationToken>,
    /// Pull-based paging: deliver this many rows after each result-set header, then pause
    /// until [`RunControl::fetch`]; `None` (default): stream every row (still bounded by the
    /// budget and back-pressure).
    pub page_rows: Option<u32>,
}

impl fmt::Debug for RunRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RunRequest")
            .field("text_len", &self.text.len())
            .field("offset", &self.offset)
            .field("kind", &self.kind)
            .field("params", &self.params.len())
            .field("timeout", &self.timeout)
            .field("resume_from", &self.resume_from)
            .field("confirmed", &self.confirmation.is_some())
            .finish_non_exhaustive()
    }
}

impl RunRequest {
    fn new(text: String, offset: usize, kind: RunKind) -> Self {
        Self {
            text,
            offset,
            kind,
            language: QueryLanguage::Sql,
            params: Vec::new(),
            timeout: TimeoutChoice::Default,
            budget: None,
            on_error: None,
            resume_from: None,
            confirmation: None,
            page_rows: None,
        }
    }

    /// One statement.
    #[must_use]
    pub fn statement(text: impl Into<String>) -> Self {
        Self::new(text.into(), 0, RunKind::Statement)
    }

    /// Every statement of a script.
    #[must_use]
    pub fn script(text: impl Into<String>) -> Self {
        Self::new(text.into(), 0, RunKind::Script)
    }

    /// Every statement of a selection starting at byte `offset` of the editor buffer.
    #[must_use]
    pub fn selection(text: impl Into<String>, offset: usize) -> Self {
        Self::new(text.into(), offset, RunKind::Script)
    }

    /// Sets bind parameters (single statement only).
    #[must_use]
    pub fn with_params(mut self, params: Vec<Param>) -> Self {
        self.params = params;
        self
    }

    /// Sets the timeout.
    #[must_use]
    pub fn with_timeout(mut self, timeout: TimeoutChoice) -> Self {
        self.timeout = timeout;
        self
    }

    /// Sets the budget.
    #[must_use]
    pub fn with_budget(mut self, budget: FetchBudget) -> Self {
        self.budget = Some(budget);
        self
    }

    /// Sets the behaviour after an error.
    #[must_use]
    pub fn with_on_error(mut self, on_error: OnError) -> Self {
        self.on_error = Some(on_error);
        self
    }

    /// Sets the language.
    #[must_use]
    pub fn with_language(mut self, language: QueryLanguage) -> Self {
        self.language = language;
        self
    }

    /// Pull-based paging for the grid (P3-B): `rows` rows after each result-set header, then
    /// [`RunEvent::PageEnd`] and a pause until [`RunControl::fetch`] or
    /// [`RunControl::release`]. A paused result keeps its stream open, so the session is busy
    /// (other statements and transaction commands wait) until the result is fetched to its end
    /// or released. In a script, every statement but the last is released after its first page
    /// ([`RunEvent::StatementClosed`]) so that the next one can run; the last one stays open.
    /// **Only statements the dialect classifies as reads are paged or released**: a release is
    /// a server-side cancel, which would roll back a write (`DELETE … RETURNING`, `CALL`), so
    /// any other statement streams to its end under the budget, and `release` is ignored for
    /// it. A released statement ends with [`RunEvent::StatementClosed`] only when the engine
    /// confirms the cancel; an engine error that arrived meanwhile is
    /// [`RunEvent::StatementFailed`].
    /// Choose `FetchBudget::batch_rows` no larger than `rows`: a page ends on a batch boundary.
    #[must_use]
    pub fn with_paging(mut self, rows: u32) -> Self {
        self.page_rows = Some(rows);
        self
    }

    /// Starts at byte `offset` of the editor buffer (the `span.start` of a
    /// [`RunEvent::NeedsConfirmation`]): `text` from there on is split with the session's
    /// current lexical modes and run; nothing before it runs again. An offset outside the
    /// text or not on a character boundary refuses the run.
    #[must_use]
    pub fn resume_from(mut self, offset: usize) -> Self {
        self.resume_from = Some(offset);
        self
    }

    /// Attaches the user's confirmation ([`RunEvent::NeedsConfirmation`]). It is given to the
    /// first statement of the run only, and only when that statement starts exactly at
    /// [`Self::resume_from`] (or, without it, to the first statement of the text); the session
    /// also checks that the token was issued for exactly that text.
    #[must_use]
    pub fn confirmed(mut self, token: ConfirmationToken) -> Self {
        self.confirmation = Some(token);
        self
    }
}

/// Where an engine error points (F-SQL-12).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ErrorPosition {
    /// Byte offset in the editor buffer ([`RunRequest::offset`] included).
    pub offset: usize,
    /// 1-based line in [`RunRequest::text`].
    pub line: u32,
    /// 1-based column (in characters) in that line.
    pub column: u32,
}

/// How a run ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RunEnd {
    /// Every statement ran (some may have failed under [`OnError::Continue`]).
    Completed,
    /// Stopped at the failed statement `index` ([`OnError::Stop`]).
    Failed {
        /// Statement index.
        index: usize,
    },
    /// Cancelled by the user, during statement `index` or before it started.
    Cancelled {
        /// Statement index.
        index: usize,
    },
    /// Stopped at statement `index`, which exceeded its timeout.
    TimedOut {
        /// Statement index.
        index: usize,
    },
    /// Stopped before statement `index`, which needs confirmation (resume with the token).
    NeedsConfirmation {
        /// Statement index.
        index: usize,
    },
    /// The request was invalid or the session is closed; nothing more ran.
    Refused,
}

/// Summary of a run ([`RunEvent::Done`]).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct RunSummary {
    /// Items in the text (statements and client directives), as split last: after a
    /// statement that may change the lexical modes, the rest of the text is split again.
    pub statements: usize,
    /// Statements sent.
    pub executed: usize,
    /// Statements that finished without error.
    pub succeeded: usize,
    /// Statements that failed.
    pub failed: usize,
    /// How it ended.
    pub end: RunEnd,
    /// Wall-clock time of the whole run.
    pub elapsed: Duration,
    /// Transaction state of the session at the end.
    pub transaction: TransactionState,
}

/// Event of one run. Exactly one [`RunEvent::Done`] ends it. `index` is the 0-based index of
/// the item in the request text as the dialect splits it (client directives, never sent, keep
/// their index). Indexes are per run: after a `Session` or `ChangesAccessMode` statement the rest
/// of the text is split again with the session's lexical modes as they are then (review H2), so
/// the indexes that follow come from that split. Resume by offset
/// ([`RunRequest::resume_from`]), never by index.
#[derive(Debug)]
#[non_exhaustive]
pub enum RunEvent {
    /// The run started: the text holds `statements` statements (first event).
    Started {
        /// Statements in the text.
        statements: usize,
    },
    /// Statement `index` was sent.
    StatementStarted {
        /// Statement index.
        index: usize,
        /// Its byte range in the editor buffer.
        span: Range<usize>,
        /// Execution id in the session, when the driver records it.
        execution: Option<ExecutionId>,
    },
    /// The previous statement was cancelled and the server has not confirmed its end yet.
    WaitingForPreviousCancel {
        /// Statement index.
        index: usize,
    },
    /// One result item of statement `index`, unchanged from the driver: a result-set header,
    /// a batch of rows, documents, an update count, a notice, or `Truncated` (the budget was
    /// reached; the result is incomplete).
    Item {
        /// Statement index.
        index: usize,
        /// The item.
        item: ResultItem,
    },
    /// Paging ([`RunRequest::with_paging`]): statement `index` delivered the page it was
    /// asked for and its result is paused, stream open. Ask for more with
    /// [`RunControl::fetch`] or close it with [`RunControl::release`]. Not sent when the
    /// result ended exactly at the page end ([`RunEvent::StatementFinished`] follows).
    PageEnd {
        /// Statement index.
        index: usize,
        /// Rows / documents delivered so far by this statement.
        rows: u64,
    },
    /// Statement `index`'s stream was closed early: released ([`RunControl::release`]), or a
    /// script moved on to the next statement after its first page. Its result keeps the rows
    /// delivered. Counted as succeeded.
    StatementClosed {
        /// Statement index.
        index: usize,
        /// Rows / documents delivered.
        rows: u64,
    },
    /// Statement `index` ended normally.
    StatementFinished {
        /// Statement index.
        index: usize,
        /// Rows / documents delivered.
        rows: u64,
        /// Estimated bytes delivered.
        bytes: u64,
        /// Sum of the update counts reported, if any.
        update_count: Option<u64>,
        /// Wall-clock time.
        elapsed: Duration,
        /// Transaction state after it.
        transaction: TransactionState,
    },
    /// Statement `index` failed.
    StatementFailed {
        /// Statement index.
        index: usize,
        /// The error (display only; engine messages may quote values).
        error: CoreError,
        /// Where it points, when the engine reports a position.
        position: Option<ErrorPosition>,
        /// Wall-clock time.
        elapsed: Duration,
    },
    /// Statement `index` was cancelled by the user.
    StatementCancelled {
        /// Statement index.
        index: usize,
        /// Wall-clock time.
        elapsed: Duration,
    },
    /// Statement `index` exceeded its timeout and was cancelled.
    StatementTimedOut {
        /// Statement index.
        index: usize,
        /// The timeout.
        after: Duration,
    },
    /// Statement `index` was not sent: it needs the user's confirmation.
    NeedsConfirmation {
        /// Statement index.
        index: usize,
        /// Its byte range in the editor buffer: resume with
        /// [`RunRequest::resume_from`]`(span.start)` and [`RunRequest::confirmed`].
        span: Range<usize>,
        /// What to show.
        request: ConfirmationRequest,
    },
    /// The run ended (terminal).
    Done(RunSummary),
}

/// How much more of a paused result to deliver ([`RunControl::fetch`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FetchMode {
    /// About this many more rows (a page ends on a batch boundary).
    Page(u32),
    /// Everything left, up to the end of the result or its budget (never unbounded: the
    /// budget ends the result with `Truncated`).
    All,
}

/// Release requests kept until the run reads them.
const MAX_PENDING_RELEASES: usize = 64;

/// Pending paging requests, coalesced (review L3): memory stays constant however many
/// requests the UI sends before the run reads them.
#[derive(Debug, Default)]
struct ControlState {
    /// Statement the pending fetch is for; a fetch for another statement replaces it.
    fetch_for: Option<usize>,
    /// Extra rows asked for by `Page` requests (summed, saturating).
    extra_rows: u64,
    /// An `All` request is pending.
    all: bool,
    /// Statement indexes released (at most [`MAX_PENDING_RELEASES`], the highest kept).
    release: std::collections::BTreeSet<usize>,
}

#[derive(Debug, Default)]
struct ControlShared {
    state: std::sync::Mutex<ControlState>,
    notify: tokio::sync::Notify,
}

impl ControlShared {
    fn take(&self) -> ControlState {
        std::mem::take(&mut *self.state.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

/// Paging and release of a run's results, usable from any thread; every method returns at
/// once. Cheap to clone (the grid's result source holds one). Requests are coalesced (bounded
/// memory); requests for a statement that is not the running one are ignored.
#[derive(Clone)]
pub struct RunControl {
    shared: Arc<ControlShared>,
    cancel: CancelToken,
}

impl fmt::Debug for RunControl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RunControl").finish_non_exhaustive()
    }
}

impl RunControl {
    /// Delivers more of statement `index`'s result (its next page, or all of it).
    pub fn fetch(&self, index: usize, mode: FetchMode) {
        {
            let mut st = self
                .shared
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if st.fetch_for != Some(index) {
                st.fetch_for = Some(index);
                st.extra_rows = 0;
                st.all = false;
            }
            match mode {
                FetchMode::Page(rows) => {
                    st.extra_rows = st.extra_rows.saturating_add(u64::from(rows.max(1)));
                }
                FetchMode::All => st.all = true,
            }
        }
        self.shared.notify.notify_one();
    }

    /// Closes statement `index`'s stream early (server-side cancel when the engine has one);
    /// [`RunEvent::StatementClosed`] follows, and a script goes on with its next statement.
    /// Does nothing once the statement ended.
    pub fn release(&self, index: usize) {
        {
            let mut st = self
                .shared
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            st.release.insert(index);
            while st.release.len() > MAX_PENDING_RELEASES {
                st.release.pop_first();
            }
        }
        self.shared.notify.notify_one();
    }

    /// Stops the whole run, as [`Run::cancel`].
    pub fn cancel(&self) {
        self.cancel.cancel();
    }
}

/// A running statement or script: its events, its cancel path and its paging control.
/// Dropping it cancels the run.
pub struct Run {
    events: mpsc::Receiver<RunEvent>,
    cancel: CancelToken,
    control: RunControl,
}

impl fmt::Debug for Run {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Run")
            .field("cancelled", &self.cancel.is_cancelled())
            .finish_non_exhaustive()
    }
}

impl Run {
    /// Stops the run ("Stop", F-SQL-05): the running statement is cancelled server-side when
    /// the engine has a cancel, and no further statement starts. Never blocks; idempotent.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// Paging and release ([`RunRequest::with_paging`]); clone it into the grid's result
    /// source.
    #[must_use]
    pub fn control(&self) -> RunControl {
        self.control.clone()
    }

    /// Shortcut for [`RunControl::fetch`].
    pub fn fetch(&self, index: usize, mode: FetchMode) {
        self.control.fetch(index, mode);
    }

    /// Shortcut for [`RunControl::release`].
    pub fn release(&self, index: usize) {
        self.control.release(index);
    }

    /// Next event; `None` after [`RunEvent::Done`].
    pub async fn next(&mut self) -> Option<RunEvent> {
        self.events.recv().await
    }

    /// Next event without waiting.
    pub fn try_next(&mut self) -> Next<RunEvent> {
        match self.events.try_recv() {
            Ok(e) => Next::Ready(e),
            Err(mpsc::error::TryRecvError::Empty) => Next::Pending,
            Err(mpsc::error::TryRecvError::Disconnected) => Next::Ended,
        }
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

struct ServiceInner {
    manager: SessionManager,
    history: Option<History>,
    prefs: RwLock<Preferences>,
}

/// The execution service. Cheap to clone. Built by [`crate::CoreServices`]
/// ([`crate::CoreServices::execution`]), or with [`ExecutionService::new`] in tests.
#[derive(Clone)]
pub struct ExecutionService {
    inner: Arc<ServiceInner>,
}

impl fmt::Debug for ExecutionService {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExecutionService")
            .field("history", &self.inner.history.is_some())
            .finish_non_exhaustive()
    }
}

impl ExecutionService {
    /// A service over `manager`, recording into `history` (if any), with `prefs`. Applies the
    /// keep-alive interval of `prefs` to the manager.
    #[must_use]
    pub fn new(manager: SessionManager, history: Option<History>, prefs: Preferences) -> Self {
        manager.set_keep_alive(prefs.execution.keep_alive);
        Self {
            inner: Arc::new(ServiceInner {
                manager,
                history,
                prefs: RwLock::new(prefs),
            }),
        }
    }

    /// The current preferences.
    #[must_use]
    pub fn preferences(&self) -> Preferences {
        self.inner
            .prefs
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Applies new preferences to the next runs, the keep-alive of every session and the
    /// history (enabled flag, retention). Does not write them ([`crate::CoreServices::save_preferences`]
    /// does both).
    pub fn set_preferences(&self, prefs: Preferences) {
        self.inner
            .manager
            .set_keep_alive(prefs.execution.keep_alive);
        if let Some(h) = &self.inner.history {
            h.set_preferences(&prefs.history);
        }
        *self
            .inner
            .prefs
            .write()
            .unwrap_or_else(PoisonError::into_inner) = prefs;
    }

    /// The query history, when it could be opened.
    #[must_use]
    pub fn history(&self) -> Option<&History> {
        self.inner.history.as_ref()
    }

    /// The statements of `text` as the dialect of `engine` splits them (for the editor: the
    /// statement under the cursor, statement markers). Pure, never blocks.
    ///
    /// # Errors
    /// [`CoreError::NoDriver`] when no driver serves `engine`.
    /// Uses the engine's default lexical modes (no session): [`SessionHandle::dialect`] gives
    /// the modes of an open session.
    pub fn split(&self, engine: &EngineId, text: &str) -> Result<Vec<StatementSpan>, CoreError> {
        Ok(self
            .inner
            .manager
            .registry()
            .dialect(engine)?
            .split_statements(text))
    }

    /// The statement at byte `cursor` of `text` ("Execute statement" with no selection): the
    /// one containing the cursor, else the closest one before it on the same or a previous
    /// line, else the next one. `None` when the text holds no statement.
    ///
    /// # Errors
    /// [`CoreError::NoDriver`].
    pub fn statement_at(
        &self,
        engine: &EngineId,
        text: &str,
        cursor: usize,
    ) -> Result<Option<StatementSpan>, CoreError> {
        Ok(pick_statement(&self.split(engine, text)?, cursor))
    }

    /// Runs `request` on `session`. Never blocks: the work runs on the driver runtime.
    #[must_use]
    pub fn run(&self, session: &SessionHandle, request: RunRequest) -> Run {
        let (tx, events) = mpsc::channel(RUN_QUEUE + 1);
        let control = Arc::new(ControlShared::default());
        let cancel = CancelToken::new();
        let prefs = self.preferences();
        let job = RunJob {
            session: session.clone(),
            history: self.inner.history.clone(),
            out: RunOutput::new(tx, cancel.clone()),
            cancel: cancel.clone(),
            control: Arc::clone(&control),
            started: Instant::now(),
        };
        // The session's own dialect: drivers keep its lexical rules in sync with the session's
        // modes (`standard_conforming_strings`, `NO_BACKSLASH_ESCAPES`).
        let dialect = Ok(session.dialect());
        self.inner.manager.runtime().spawn(async move {
            job.run(request, dialect, &prefs).await;
        });
        Run {
            events,
            control: RunControl {
                shared: control,
                cancel: cancel.clone(),
            },
            cancel,
        }
    }
}

/// The error a cancelled statement ends with: [`DriverError::Cancelled`], or the engine's own
/// "cancelled" error (SQLSTATE `57014`, PostgreSQL `query_canceled`).
fn is_cancel_error(error: &CoreError) -> bool {
    match error {
        CoreError::Driver(DriverError::Cancelled) => true,
        CoreError::Driver(DriverError::Query { code, .. }) => code.as_deref() == Some("57014"),
        _ => false,
    }
}

/// Kind of one item of a script, as the execution service runs it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ItemKind {
    /// Sent with one `Session::execute` call.
    Statement,
    /// A client-side directive (`DELIMITER`, `\\set`…): never sent to the engine.
    // Built once `split_script` lands (P3-D integration point in `script_items`).
    #[allow(dead_code)]
    ClientDirective,
    /// Something the dialect refuses to run as part of a script: the whole script is refused
    /// up front.
    Refused,
}

/// One item of a script.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ScriptItem {
    span: StatementSpan,
    kind: ItemKind,
}

/// The items of `text` for `dialect`.
///
/// Splits `text[from..]` (`from` on a `char` boundary, the end of the statement that ran last)
/// and returns spans absolute in `text`: the execution service splits the rest of a script
/// again after a statement that may change the lexical modes (review H2).
///
/// INTEGRATION POINT (P3-D, `feat/p3-d-dialects`): replace the body with
/// `dialect.split_script(&text[from..])` (shifting every span by `from`), mapping `ScriptItemKind::Statement` to
/// [`ItemKind::Statement`], `ClientDirective` to [`ItemKind::ClientDirective`], `Refused(_)`
/// and any future kind to [`ItemKind::Refused`] (fail-closed), and compute error positions with
/// `LineIndex::new(text).position(item.span.start + offset)` in [`error_position`]. Until then
/// every span of `split_statements` is a statement.
fn script_items(
    dialect: &dyn ninesql_driver_api::Dialect,
    text: &str,
    from: usize,
) -> Vec<ScriptItem> {
    let rest = text.get(from..).unwrap_or("");
    dialect
        .split_statements(rest)
        .into_iter()
        .map(|span| ScriptItem {
            span: StatementSpan {
                start: span.start + from,
                end: span.end + from,
            },
            kind: ItemKind::Statement,
        })
        .collect()
}

/// Checks the items of a request before anything runs; answers the index of the last
/// statement. A refused item refuses the whole script (recommended by the dialect contract, and
/// the safe choice for production); "Execute statement" and bind parameters need a single
/// statement.
fn check_items(items: &[ScriptItem], kind: RunKind, has_params: bool) -> Result<usize, CoreError> {
    if items.iter().any(|i| i.kind == ItemKind::Refused) {
        return Err(CoreError::InvalidRequest(
            "the script holds a statement that cannot run as part of a script",
        ));
    }
    let statements = items
        .iter()
        .filter(|i| i.kind == ItemKind::Statement)
        .count();
    if kind == RunKind::Statement && statements > 1 {
        return Err(CoreError::InvalidRequest(
            "the text holds several statements: run it as a script",
        ));
    }
    if has_params && statements > 1 {
        return Err(CoreError::InvalidRequest(
            "bind parameters need a single statement",
        ));
    }
    Ok(items
        .iter()
        .rposition(|i| i.kind == ItemKind::Statement)
        .unwrap_or(0))
}

/// See [`ExecutionService::statement_at`].
fn pick_statement(spans: &[StatementSpan], cursor: usize) -> Option<StatementSpan> {
    if let Some(s) = spans.iter().find(|s| s.start <= cursor && cursor <= s.end) {
        return Some(*s);
    }
    spans
        .iter()
        .rev()
        .find(|s| s.end <= cursor)
        .or_else(|| spans.iter().find(|s| s.start >= cursor))
        .copied()
}

/// Error position of a 1-based character offset `char_pos` in the statement at `span` of
/// `text`.
fn error_position(text: &str, span: StatementSpan, char_pos: u32, base: usize) -> ErrorPosition {
    let statement = span.text(text);
    let skip = usize::try_from(char_pos.saturating_sub(1)).unwrap_or(usize::MAX);
    let in_statement = statement
        .char_indices()
        .nth(skip)
        .map_or(statement.len(), |(i, _)| i);
    let at = (span.start + in_statement).min(text.len());
    let before = text.get(..at).unwrap_or("");
    let line = before.matches('\n').count() + 1;
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    let column = text.get(line_start..at).map_or(0, |s| s.chars().count()) + 1;
    ErrorPosition {
        offset: base + at,
        line: u32::try_from(line).unwrap_or(u32::MAX),
        column: u32::try_from(column).unwrap_or(u32::MAX),
    }
}

/// Where the events of one run go: non-terminal events give up on cancel or when the UI is
/// gone; `Done` goes through a slot reserved at start.
struct RunOutput {
    events: mpsc::Sender<RunEvent>,
    cancel: CancelToken,
    terminal: Option<mpsc::OwnedPermit<RunEvent>>,
}

impl RunOutput {
    fn new(events: mpsc::Sender<RunEvent>, cancel: CancelToken) -> Self {
        let terminal = events.clone().try_reserve_owned().ok();
        Self {
            events,
            cancel,
            terminal,
        }
    }

    fn gone(&self) -> bool {
        self.events.is_closed()
    }

    /// `false` when not delivered (cancelled while the queue was full, or the UI is gone).
    async fn send(&self, event: RunEvent) -> bool {
        tokio::select! {
            biased;
            sent = self.events.send(event) => sent.is_ok(),
            () = self.cancel.cancelled() => false,
            () = self.events.closed() => false,
        }
    }

    /// A statement end: delivered unless the queue is full after a cancel.
    async fn send_end(&self, event: RunEvent) {
        if self.cancel.is_cancelled() {
            let _ = self.events.try_send(event);
        } else {
            let _ = self.send(event).await;
        }
    }

    fn done(&mut self, summary: RunSummary) {
        let event = RunEvent::Done(summary);
        match self.terminal.take() {
            Some(permit) => {
                let _ = permit.send(event);
            }
            None => {
                let _ = self.events.try_send(event);
            }
        }
    }
}

struct RunJob {
    session: SessionHandle,
    history: Option<History>,
    out: RunOutput,
    cancel: CancelToken,
    control: Arc<ControlShared>,
    started: Instant,
}

/// One statement of a run, as [`RunJob::statement`] runs it.
#[derive(Clone, Copy)]
struct Step {
    /// Item index.
    index: usize,
    /// Absolute span in the request text.
    span: StatementSpan,
    /// The last statement of the text (a paged result stays open).
    last: bool,
    /// Classified as a read just before it runs (only reads are paged or released).
    is_read: bool,
}

/// What one statement did.
enum StatementEnd {
    Finished,
    /// Released early (the grid's release, or a script moving on after the first page).
    Closed,
    Failed,
    Cancelled,
    TimedOut,
    NeedsConfirmation,
}

impl RunJob {
    fn summary(&self, statements: usize, counts: (usize, usize, usize), end: RunEnd) -> RunSummary {
        RunSummary {
            statements,
            executed: counts.0,
            succeeded: counts.1,
            failed: counts.2,
            end,
            elapsed: self.started.elapsed(),
            transaction: self.session.transaction_state(),
        }
    }

    fn refuse(mut self, error: CoreError, statements: usize) {
        let index = 0;
        let _ = self.out.events.try_send(RunEvent::StatementFailed {
            index,
            error,
            position: None,
            elapsed: Duration::ZERO,
        });
        let summary = self.summary(statements, (0, 0, 0), RunEnd::Refused);
        self.out.done(summary);
    }

    async fn run(
        mut self,
        request: RunRequest,
        dialect: Result<Arc<dyn ninesql_driver_api::Dialect>, CoreError>,
        prefs: &Preferences,
    ) {
        let dialect = match dialect {
            Ok(d) => d,
            Err(e) => return self.refuse(e, 0),
        };
        // Where the run starts in `text` (review N1: by offset, never by index).
        let from = match request.resume_from {
            None => 0,
            Some(offset) => match offset.checked_sub(request.offset) {
                Some(from) if request.text.is_char_boundary(from) => from,
                _ => {
                    return self.refuse(
                        CoreError::InvalidRequest(
                            "the resume offset is not a position in the text",
                        ),
                        0,
                    );
                }
            },
        };
        let mut items = script_items(&*dialect, &request.text, from);
        let mut last_statement = match check_items(&items, request.kind, !request.params.is_empty())
        {
            Ok(last) => last,
            Err(e) => return self.refuse(e, items.len()),
        };
        let info = self.session.info().clone();
        let timeout = match request.timeout {
            TimeoutChoice::Default => prefs.execution.timeout_for(info.connection),
            TimeoutChoice::None => None,
            TimeoutChoice::After(d) => Some(d),
        };
        let budget = request.budget.unwrap_or_else(|| prefs.execution.budget());
        let on_error = request.on_error.unwrap_or(prefs.execution.on_error);

        let _ = self
            .out
            .send(RunEvent::Started {
                statements: items.len(),
            })
            .await;
        let (mut executed, mut succeeded, mut failed) = (0usize, 0usize, 0usize);
        let mut end = RunEnd::Completed;
        let mut index = 0;
        let mut first_statement = true;
        while let Some(item) = items.get(index).copied() {
            let span = &item.span;
            if item.kind != ItemKind::Statement {
                index += 1;
                // Client directives are never sent.
                continue;
            }
            if self.cancel.is_cancelled() || self.out.gone() {
                end = RunEnd::Cancelled { index };
                break;
            }
            let text = span.text(&request.text).to_owned();
            let mut exec_request = ExecuteRequest::sql(text.clone())
                .with_language(request.language)
                .with_params(request.params.clone())
                .with_timeout(timeout)
                .with_budget(budget);
            // The token goes to the first statement of the run only, and only when it starts
            // exactly where the run was asked to resume.
            if first_statement
                && let Some(token) = request.confirmation
                && (request.resume_from.is_none() || span.start == from)
            {
                exec_request = exec_request.confirmed(token);
            }
            first_statement = false;
            let last = index == last_statement;
            // Classified now, with the lexical modes in force now (review H2); the session task
            // classifies it again for the confirmation, at the moment it sends it. Only reads
            // may be paused and released: a release is a server-side cancel, which would roll
            // back a write (`DELETE … RETURNING`, `CALL`, DML in a transaction).
            let class = dialect.classify(&text);
            let is_read = class == StatementClass::Read;
            let step = Step {
                index,
                span: *span,
                last,
                is_read,
            };
            let outcome = self
                .statement(step, &request, exec_request, &info.engine)
                .await;
            match outcome {
                StatementEnd::Finished | StatementEnd::Closed => {
                    executed += 1;
                    succeeded += 1;
                }
                StatementEnd::NeedsConfirmation => {
                    end = RunEnd::NeedsConfirmation { index };
                    break;
                }
                StatementEnd::Cancelled => {
                    executed += 1;
                    end = RunEnd::Cancelled { index };
                    break;
                }
                StatementEnd::TimedOut => {
                    executed += 1;
                    failed += 1;
                    end = RunEnd::TimedOut { index };
                    break;
                }
                StatementEnd::Failed => {
                    executed += 1;
                    failed += 1;
                    if on_error == OnError::Stop || self.session.is_closed() {
                        end = RunEnd::Failed { index };
                        break;
                    }
                }
            }
            // A session statement may have changed the lexical modes (review H2): the rest of
            // the script is split again, from the end of this statement, with the session's
            // dialect as it is now. Offsets stay absolute.
            if matches!(
                class,
                StatementClass::Session | StatementClass::ChangesAccessMode
            ) {
                let rest = script_items(&*dialect, &request.text, span.end);
                items.truncate(index + 1);
                items.extend(rest);
                // The merged list is checked like the first split (review N2): a refused item,
                // or a "single statement" request that now holds several, stops the run here.
                match check_items(&items, request.kind, !request.params.is_empty()) {
                    Ok(last) => last_statement = last,
                    Err(error) => {
                        let _ = self.out.events.try_send(RunEvent::StatementFailed {
                            index: index + 1,
                            error,
                            position: None,
                            elapsed: Duration::ZERO,
                        });
                        end = RunEnd::Refused;
                        break;
                    }
                }
            }
            index += 1;
        }
        let summary = self.summary(items.len(), (executed, succeeded, failed), end);
        self.out.done(summary);
    }

    /// Runs one statement and forwards its events, paging its results when the request asks
    /// for it.
    #[allow(clippy::too_many_lines)] // One state machine; split, it would be harder to follow.
    async fn statement(
        &mut self,
        step: Step,
        request: &RunRequest,
        exec_request: ExecuteRequest,
        engine: &EngineId,
    ) -> StatementEnd {
        let Step {
            index,
            span,
            last,
            is_read,
        } = step;
        let params = exec_request.params.len();
        let text = exec_request.text.clone();
        let timeout = exec_request.timeout;
        let started_at = SystemTime::now();
        let started = Instant::now();
        let mut execution = self.session.execute(exec_request);
        let base = request.offset;
        // Non-reads are never paged: they stream under the budget without pausing, and are
        // never released (see `RunRequest::with_paging`).
        let page = request
            .page_rows
            .filter(|_| is_read)
            .map(|n| u64::from(n.max(1)));
        // Run cancelled ("Stop") / stream released (grid, or the script moving on).
        let mut cancel_sent = false;
        let mut releasing = false;
        let mut update_count: Option<u64> = None;
        let mut forwarding = true;
        // Rows / documents forwarded, and how many may be forwarded before pausing.
        let mut delivered: u64 = 0;
        let mut allowed: Option<u64> = None;
        let mut page_end_sent = false;
        let connection = self.session.info().connection;
        let policy = self.session.info().policy;
        let history = self.history.clone();
        let record = |status: EntryStatus,
                      rows: Option<u64>,
                      update_count: Option<u64>,
                      error: Option<&CoreError>| {
            if let Some(h) = &history {
                let outcome = h.record(
                    &policy,
                    StatementRecord {
                        rows,
                        update_count,
                        error_kind: error.map(CoreError::kind),
                        error_code: error.and_then(|e| e.engine_code().map(str::to_owned)),
                        param_count: params,
                        ..StatementRecord::new(
                            connection,
                            engine.clone(),
                            text.clone(),
                            started_at,
                            started.elapsed(),
                            status,
                        )
                    },
                );
                tracing::trace!(outcome = ?outcome, "history");
            }
        };
        loop {
            let paused = !cancel_sent && !releasing && allowed.is_some_and(|a| delivered >= a);
            if paused && !page_end_sent {
                page_end_sent = true;
                if forwarding {
                    forwarding = self
                        .out
                        .send(RunEvent::PageEnd {
                            index,
                            rows: delivered,
                        })
                        .await;
                }
                if !last {
                    // A script goes on: the statement's stream is closed after its first page
                    // (one session runs one statement at a time).
                    execution.cancel();
                    releasing = true;
                    continue;
                }
            }
            let event = tokio::select! {
                biased;
                () = self.cancel.cancelled(), if !cancel_sent => {
                    execution.cancel();
                    cancel_sent = true;
                    continue;
                }
                () = self.control.notify.notified() => {
                    let st = self.control.take();
                    if st.release.contains(&index) && is_read && !releasing && !cancel_sent {
                        execution.cancel();
                        releasing = true;
                    }
                    if st.fetch_for == Some(index) {
                        if st.all {
                            allowed = None;
                        } else if st.extra_rows > 0
                            && let Some(a) = allowed
                        {
                            allowed = Some(a.max(delivered).saturating_add(st.extra_rows));
                        }
                        page_end_sent = false;
                    }
                    continue;
                }
                e = execution.next(), if !paused => e,
            };
            let Some(event) = event else {
                // The session task ended without a terminal event (shutdown).
                self.out
                    .send_end(RunEvent::StatementFailed {
                        index,
                        error: CoreError::SessionClosed,
                        position: None,
                        elapsed: started.elapsed(),
                    })
                    .await;
                return StatementEnd::Failed;
            };
            match event {
                ExecEvent::WaitingForPreviousCancel => {
                    if forwarding {
                        forwarding = self
                            .out
                            .send(RunEvent::WaitingForPreviousCancel { index })
                            .await;
                    }
                }
                ExecEvent::Started { execution: id } => {
                    if forwarding {
                        forwarding = self
                            .out
                            .send(RunEvent::StatementStarted {
                                index,
                                span: base + span.start..base + span.end,
                                execution: id,
                            })
                            .await;
                    }
                }
                ExecEvent::Item(item) => {
                    match &item {
                        ResultItem::UpdateCount(Some(n)) => {
                            update_count = Some(update_count.unwrap_or(0).saturating_add(*n));
                        }
                        // The first page of every result set comes without being asked.
                        ResultItem::ResultSet(_) => {
                            if let Some(page) = page {
                                allowed = Some(delivered.saturating_add(page));
                                page_end_sent = false;
                            }
                        }
                        _ => {}
                    }
                    // Only rows actually handed to the UI count (not batches drained after a
                    // release or a cancel).
                    if forwarding && !releasing && !cancel_sent {
                        let n = match &item {
                            ResultItem::Rows(b) => b.num_rows() as u64,
                            ResultItem::Documents(d) => d.len() as u64,
                            _ => 0,
                        };
                        forwarding = self.out.send(RunEvent::Item { index, item }).await;
                        if forwarding {
                            delivered += n;
                        }
                    }
                }
                ExecEvent::NeedsConfirmation(request) => {
                    self.out
                        .send_end(RunEvent::NeedsConfirmation {
                            index,
                            span: base + span.start..base + span.end,
                            request,
                        })
                        .await;
                    return StatementEnd::NeedsConfirmation;
                }
                ExecEvent::Finished { rows, bytes } => {
                    let elapsed = started.elapsed();
                    record(EntryStatus::Succeeded, Some(rows), update_count, None);
                    self.out
                        .send_end(RunEvent::StatementFinished {
                            index,
                            rows,
                            bytes,
                            update_count,
                            elapsed,
                            transaction: self.session.transaction_state(),
                        })
                        .await;
                    return StatementEnd::Finished;
                }
                ExecEvent::Failed(error) => {
                    let elapsed = started.elapsed();
                    let cancel_error = is_cancel_error(&error);
                    return match error {
                        // Run cancelled by the user ("Stop"), and the engine confirmed a cancel
                        // (`Cancelled`, or SQLSTATE 57014). Any other error stays an error.
                        _ if cancel_sent && cancel_error => {
                            record(EntryStatus::Cancelled, Some(delivered), update_count, None);
                            self.out
                                .send_end(RunEvent::StatementCancelled { index, elapsed })
                                .await;
                            StatementEnd::Cancelled
                        }
                        // Stream released early (grid or script, reads only) and the engine
                        // confirmed the cancel: the rows read were what the user asked for. A
                        // real engine error that arrived meanwhile is reported as a failure.
                        _ if releasing && cancel_error => {
                            record(EntryStatus::Succeeded, Some(delivered), update_count, None);
                            self.out
                                .send_end(RunEvent::StatementClosed {
                                    index,
                                    rows: delivered,
                                })
                                .await;
                            StatementEnd::Closed
                        }
                        CoreError::Driver(DriverError::Cancelled) => {
                            record(EntryStatus::Cancelled, Some(delivered), update_count, None);
                            self.out
                                .send_end(RunEvent::StatementCancelled { index, elapsed })
                                .await;
                            StatementEnd::Cancelled
                        }
                        CoreError::Driver(DriverError::Timeout(after)) => {
                            record(EntryStatus::TimedOut, Some(delivered), update_count, None);
                            self.out
                                .send_end(RunEvent::StatementTimedOut {
                                    index,
                                    after: timeout.unwrap_or(after),
                                })
                                .await;
                            StatementEnd::TimedOut
                        }
                        error => {
                            // Not recorded: the statement never reached the engine.
                            let refused = matches!(
                                error,
                                CoreError::InvalidConfirmation
                                    | CoreError::SessionClosed
                                    | CoreError::StillCancelling
                                    | CoreError::Driver(DriverError::Busy)
                            );
                            if !refused {
                                record(EntryStatus::Failed, None, update_count, Some(&error));
                            }
                            let position = match &error {
                                CoreError::Driver(DriverError::Query {
                                    position: Some(p), ..
                                }) => Some(error_position(&request.text, span, *p, base)),
                                _ => None,
                            };
                            self.out
                                .send_end(RunEvent::StatementFailed {
                                    index,
                                    error,
                                    position,
                                    elapsed,
                                })
                                .await;
                            StatementEnd::Failed
                        }
                    };
                }
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).
mod tests {
    use super::*;

    fn span(start: usize, end: usize) -> StatementSpan {
        StatementSpan { start, end }
    }

    /// Review H2: the rest of a script is split from an offset, spans stay absolute.
    #[test]
    fn script_items_split_the_rest_with_absolute_offsets() {
        let d = ninesql_driver_api::AnsiDialect;
        let text = "SET x = 1;\nSELECT 1;\nSELECT 2";
        let all = script_items(&d, text, 0);
        assert_eq!(all.len(), 3);
        let rest = script_items(&d, text, all[0].span.end);
        assert_eq!(rest.len(), 2);
        assert_eq!(rest[0].span.text(text), "SELECT 1");
        assert_eq!(rest[1].span.text(text), "SELECT 2");
        assert!(script_items(&d, text, text.len()).is_empty());
        assert!(script_items(&d, text, text.len() + 5).is_empty());
    }

    #[test]
    fn script_items_are_checked_before_anything_runs() {
        let item = |kind| ScriptItem {
            span: span(0, 1),
            kind,
        };
        let (st, dir, refused) = (
            item(ItemKind::Statement),
            item(ItemKind::ClientDirective),
            item(ItemKind::Refused),
        );
        assert_eq!(
            check_items(&[st, dir, st, dir], RunKind::Script, false).unwrap(),
            2
        );
        // A directive does not count as a statement.
        assert_eq!(
            check_items(&[dir, st], RunKind::Statement, true).unwrap(),
            1
        );
        assert!(check_items(&[st, st], RunKind::Statement, false).is_err());
        assert!(check_items(&[st, st], RunKind::Script, true).is_err());
        assert!(matches!(
            check_items(&[st, refused, st], RunKind::Script, false),
            Err(CoreError::InvalidRequest(_))
        ));
        assert_eq!(check_items(&[], RunKind::Script, false).unwrap(), 0);
    }

    #[test]
    fn picks_the_statement_under_the_cursor() {
        // "SELECT 1;\nSELECT 2;\n\nSELECT 3"
        let spans = [span(0, 8), span(10, 18), span(21, 29)];
        assert_eq!(pick_statement(&spans, 3), Some(span(0, 8)));
        assert_eq!(
            pick_statement(&spans, 8),
            Some(span(0, 8)),
            "right after it"
        );
        assert_eq!(pick_statement(&spans, 9), Some(span(0, 8)), "after the `;`");
        assert_eq!(pick_statement(&spans, 20), Some(span(10, 18)));
        assert_eq!(pick_statement(&spans, 100), Some(span(21, 29)));
        assert_eq!(
            pick_statement(&[span(5, 9)], 0),
            Some(span(5, 9)),
            "the next one"
        );
        assert_eq!(pick_statement(&[], 0), None);
    }

    #[test]
    fn error_positions_point_into_the_buffer() {
        let text = "SELECT 1;\nSELECT é,\n  nope FROM t";
        let s = span(10, text.len());
        // 1-based char 13 of the statement is `n` of `nope` (`SELECT é,\n  ` is 12 chars).
        let p = error_position(text, s, 13, 100);
        assert_eq!(&text[p.offset - 100..p.offset + 4 - 100], "nope");
        assert_eq!((p.line, p.column), (3, 3));
        // Out of range: the end of the statement.
        let p = error_position(text, s, 10_000, 0);
        assert_eq!(p.offset, text.len());
        let p = error_position(text, span(0, 8), 1, 0);
        assert_eq!((p.offset, p.line, p.column), (0, 1, 1));
    }
}
