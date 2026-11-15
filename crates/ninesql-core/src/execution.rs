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
//! - **Events.** Each statement reports [`RunEvent::StatementStarted`], its result items
//!   ([`RunEvent::ResultSet`], [`RunEvent::Rows`], [`RunEvent::Documents`],
//!   [`RunEvent::UpdateCount`], [`RunEvent::Notice`], [`RunEvent::Truncated`]) and exactly one
//!   statement end: [`RunEvent::StatementFinished`], [`RunEvent::StatementFailed`] (with an
//!   [`ErrorPosition`] when the engine reports one), [`RunEvent::StatementCancelled`],
//!   [`RunEvent::StatementTimedOut`] or [`RunEvent::NeedsConfirmation`]. The run ends with
//!   exactly one [`RunEvent::Done`], which is never dropped.
//! - **Back-pressure.** Events go through a small bounded channel: a UI that reads slowly slows
//!   the fetch down (on top of the result's own budget). After a cancel, result items still
//!   queued are dropped; the statement end and `Done` still arrive.
//! - **Confirmation.** A statement that needs confirmation is not sent: the run ends with
//!   [`RunEnd::NeedsConfirmation`]. The UI shows the dialog and calls `run` again with the same
//!   text, [`RunRequest::resume_at`] the statement's index and the token
//!   ([`RunRequest::confirmed`]): statements before it are not run again.
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

use ninesql_driver_api::arrow_array::RecordBatch;
use ninesql_driver_api::{
    CancelToken, Document, DriverError, ExecutionId, FetchBudget, Notice, Param, QueryLanguage,
    ResultItem, ResultSetHeader, StatementSpan, Truncation,
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
    /// Index of the first statement to run (resuming after a confirmation).
    pub start_at: usize,
    /// Confirmation of the statement at this index.
    pub confirmation: Option<(usize, ConfirmationToken)>,
}

impl fmt::Debug for RunRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RunRequest")
            .field("text_len", &self.text.len())
            .field("offset", &self.offset)
            .field("kind", &self.kind)
            .field("params", &self.params.len())
            .field("timeout", &self.timeout)
            .field("start_at", &self.start_at)
            .field("confirmed", &self.confirmation.map(|(i, _)| i))
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
            start_at: 0,
            confirmation: None,
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

    /// Starts at statement `index` (0-based), skipping the ones before.
    #[must_use]
    pub fn resume_at(mut self, index: usize) -> Self {
        self.start_at = index;
        self
    }

    /// Attaches the user's confirmation of statement `index`
    /// ([`RunEvent::NeedsConfirmation`]).
    #[must_use]
    pub fn confirmed(mut self, index: usize, token: ConfirmationToken) -> Self {
        self.confirmation = Some((index, token));
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
    /// Statements in the text.
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

/// Event of one run. Exactly one [`RunEvent::Done`] ends it. `index` is the 0-based statement
/// index in the request text.
#[derive(Debug)]
#[non_exhaustive]
pub enum RunEvent {
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
    /// A result set starts.
    ResultSet {
        /// Statement index.
        index: usize,
        /// Header.
        header: ResultSetHeader,
    },
    /// A batch of rows of the current result set.
    Rows {
        /// Statement index.
        index: usize,
        /// Rows.
        batch: RecordBatch,
    },
    /// Documents of the current result set.
    Documents {
        /// Statement index.
        index: usize,
        /// Documents.
        documents: Vec<Document>,
    },
    /// Rows affected.
    UpdateCount {
        /// Statement index.
        index: usize,
        /// Count, when the engine reports one.
        count: Option<u64>,
    },
    /// A server notice or warning.
    Notice {
        /// Statement index.
        index: usize,
        /// Notice.
        notice: Notice,
    },
    /// The result reached its budget and is incomplete.
    Truncated {
        /// Statement index.
        index: usize,
        /// Which limit.
        truncation: Truncation,
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
        /// What to show; resume with [`RunRequest::resume_at`] and [`RunRequest::confirmed`].
        request: ConfirmationRequest,
    },
    /// The run ended (terminal).
    Done(RunSummary),
}

/// A running statement or script: its events and its cancel path. Dropping it cancels the run.
pub struct Run {
    events: mpsc::Receiver<RunEvent>,
    cancel: CancelToken,
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
        let cancel = CancelToken::new();
        let prefs = self.preferences();
        let job = RunJob {
            session: session.clone(),
            history: self.inner.history.clone(),
            out: RunOutput::new(tx, cancel.clone()),
            cancel: cancel.clone(),
            started: Instant::now(),
        };
        let dialect = self
            .inner
            .manager
            .registry()
            .dialect(&session.info().engine);
        self.inner.manager.runtime().spawn(async move {
            job.run(request, dialect, &prefs).await;
        });
        Run { events, cancel }
    }
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
    started: Instant,
}

/// What one statement did.
enum StatementEnd {
    Finished,
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
        let spans = dialect.split_statements(&request.text);
        let total = spans.len();
        if request.kind == RunKind::Statement && total > 1 {
            return self.refuse(
                CoreError::InvalidRequest("the text holds several statements: run it as a script"),
                total,
            );
        }
        if !request.params.is_empty() && total > 1 {
            return self.refuse(
                CoreError::InvalidRequest("bind parameters need a single statement"),
                total,
            );
        }
        let info = self.session.info().clone();
        let timeout = match request.timeout {
            TimeoutChoice::Default => prefs.execution.timeout_for(info.connection),
            TimeoutChoice::None => None,
            TimeoutChoice::After(d) => Some(d),
        };
        let budget = request.budget.unwrap_or_else(|| prefs.execution.budget());
        let on_error = request.on_error.unwrap_or(prefs.execution.on_error);

        let (mut executed, mut succeeded, mut failed) = (0usize, 0usize, 0usize);
        let mut end = RunEnd::Completed;
        for (index, span) in spans.iter().enumerate().skip(request.start_at) {
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
            if let Some((i, token)) = request.confirmation
                && i == index
            {
                exec_request = exec_request.confirmed(token);
            }
            let outcome = self
                .statement(index, *span, &request, exec_request, &info.engine)
                .await;
            match outcome {
                StatementEnd::Finished => {
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
        }
        let summary = self.summary(total, (executed, succeeded, failed), end);
        self.out.done(summary);
    }

    /// Runs one statement and forwards its events.
    async fn statement(
        &self,
        index: usize,
        span: StatementSpan,
        request: &RunRequest,
        exec_request: ExecuteRequest,
        engine: &EngineId,
    ) -> StatementEnd {
        let params = exec_request.params.len();
        let text = exec_request.text.clone();
        let timeout = exec_request.timeout;
        let started_at = SystemTime::now();
        let started = Instant::now();
        let mut execution = self.session.execute(exec_request);
        let mut cancel_sent = false;
        let mut update_count: Option<u64> = None;
        let mut forwarding = true;
        let base = request.offset;
        let record = |status: EntryStatus,
                      rows: Option<u64>,
                      update_count: Option<u64>,
                      error: Option<&CoreError>| {
            StatementRecord {
                rows,
                update_count,
                error_kind: error.map(CoreError::kind),
                error_code: error.and_then(|e| e.engine_code().map(str::to_owned)),
                param_count: params,
                ..StatementRecord::new(
                    self.session.info().connection,
                    engine.clone(),
                    text.clone(),
                    started_at,
                    started.elapsed(),
                    status,
                )
            }
        };
        loop {
            let event = if cancel_sent {
                execution.next().await
            } else {
                tokio::select! {
                    biased;
                    e = execution.next() => e,
                    () = self.cancel.cancelled() => {
                        execution.cancel();
                        cancel_sent = true;
                        continue;
                    }
                }
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
            let forward = |item: ResultItem| -> Option<RunEvent> {
                Some(match item {
                    ResultItem::ResultSet(header) => RunEvent::ResultSet { index, header },
                    ResultItem::Rows(batch) => RunEvent::Rows { index, batch },
                    ResultItem::Documents(documents) => RunEvent::Documents { index, documents },
                    ResultItem::UpdateCount(count) => RunEvent::UpdateCount { index, count },
                    ResultItem::Notice(notice) => RunEvent::Notice { index, notice },
                    ResultItem::Truncated(truncation) => RunEvent::Truncated { index, truncation },
                    _ => return None,
                })
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
                    if let ResultItem::UpdateCount(Some(n)) = &item {
                        update_count = Some(update_count.unwrap_or(0).saturating_add(*n));
                    }
                    if forwarding && let Some(e) = forward(item) {
                        forwarding = self.out.send(e).await;
                    }
                }
                ExecEvent::NeedsConfirmation(request) => {
                    self.out
                        .send_end(RunEvent::NeedsConfirmation { index, request })
                        .await;
                    return StatementEnd::NeedsConfirmation;
                }
                ExecEvent::Finished { rows, bytes } => {
                    let elapsed = started.elapsed();
                    self.record(record(
                        EntryStatus::Succeeded,
                        Some(rows),
                        update_count,
                        None,
                    ));
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
                    return match error {
                        CoreError::Driver(DriverError::Cancelled) => {
                            self.record(record(EntryStatus::Cancelled, None, update_count, None));
                            self.out
                                .send_end(RunEvent::StatementCancelled { index, elapsed })
                                .await;
                            StatementEnd::Cancelled
                        }
                        CoreError::Driver(DriverError::Timeout(after)) => {
                            self.record(record(EntryStatus::TimedOut, None, update_count, None));
                            self.out
                                .send_end(RunEvent::StatementTimedOut {
                                    index,
                                    after: timeout.unwrap_or(after),
                                })
                                .await;
                            StatementEnd::TimedOut
                        }
                        // A statement cancelled by the user may end with the engine's own
                        // "cancelled" error (SQLSTATE 57014): still a cancel.
                        _ if cancel_sent => {
                            self.record(record(EntryStatus::Cancelled, None, update_count, None));
                            self.out
                                .send_end(RunEvent::StatementCancelled { index, elapsed })
                                .await;
                            StatementEnd::Cancelled
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
                                self.record(record(
                                    EntryStatus::Failed,
                                    None,
                                    update_count,
                                    Some(&error),
                                ));
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

    fn record(&self, record: StatementRecord) {
        if let Some(h) = &self.history {
            let outcome = h.record(&self.session.info().policy, record);
            tracing::trace!(session = %self.session.id(), outcome = ?outcome, "history");
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
