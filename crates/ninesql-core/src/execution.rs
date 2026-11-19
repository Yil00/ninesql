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
//! - **Splitting.** A script or selection is split with the session's dialect as an
//!   "execute script" ([`ninesql_driver_api::Dialect::split_script`] contract, P3-D): lazily
//!   with `split_script_ahead` and the [`ScriptState`] (the MySQL `DELIMITER` in force) kept
//!   item by item with `script_state_after`, so the work stays linear. Client directives
//!   (`DELIMITER`) are applied, never sent. A refused piece (`COPY … FROM STDIN`, a psql
//!   meta-command, a mysql client command, an invalid `DELIMITER`) refuses the whole run before
//!   anything is sent (one lazy pass up front); a refused piece that only shows up after a
//!   re-split stops the run before its next statement. Each statement is classified with the
//!   session's dialect just before it runs, and after **every** statement the session dialect's
//!   `lexical_rules()` are compared with the ones the pending items were split with: when they
//!   differ (a `SET`, but also a `ROLLBACK` undoing one), the rest is split again from the end
//!   of that statement (review H2, P3-D L2). The driver re-checks each statement with its own
//!   session lexical modes and refuses text that still holds several statements
//!   ([`DriverError::MultipleStatements`]), so a mis-split never runs something other than what
//!   was classified.
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
//! - **Idle paused results (phase 3 review M1).** A paused result holds the session, a server
//!   cursor and often a transaction snapshot, and the drivers end a session whose result has
//!   waited too long (pgwire `IDLE_CURSOR_TIMEOUT`, mysqlwire `net_write_timeout`, both 300 s);
//!   on mysqlwire in manual commit that rolls back the user's open transaction. So a run
//!   releases a result paused with no [`RunControl::fetch`] for longer than
//!   [`crate::ExecutionPreferences::paused_result_idle`] (default 240 s, always below 300 s)
//!   itself, through the same path as [`RunControl::release`]: reads only, a server-side close
//!   or cancel of the read, never of anything else. [`RunEvent::ResultClosedIdle`] comes just
//!   before the [`RunEvent::StatementClosed`]; the rows already delivered are kept and the
//!   session is free (its transaction, if any, still open) for the next statement.
//! - **Back-pressure.** Events go through a small bounded channel: a UI that reads slowly slows
//!   the fetch down (on top of the result's own budget). After a cancel, result items still
//!   queued are dropped; the statement end and `Done` still arrive.
//! - **Confirmation.** A statement that needs confirmation is not sent: the run ends with
//!   [`RunEnd::NeedsConfirmation`]. The UI shows the dialog and calls `run` again with the same
//!   text, [`RunRequest::resume_from`] the statement's start offset
//!   ([`RunEvent::NeedsConfirmation::span`]) and the token ([`RunRequest::confirmed`]):
//!   statements before it are not run again. Resuming is by byte offset, never by index:
//!   indexes depend on the lexical modes in force when the text is split (review N1). The
//!   splitter state at that offset (the `DELIMITER` in force) is rebuilt from the text before
//!   it, or taken from [`RunEvent::NeedsConfirmation::script_state`] when the request carries
//!   it ([`RunRequest::with_script_state`], exact even when the prefix changed lexical modes).
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

use std::collections::VecDeque;
use std::fmt;
use std::ops::Range;
use std::sync::{Arc, PoisonError, RwLock};
use std::time::{Duration, Instant, SystemTime};

use ninesql_driver_api::{
    CancelToken, Dialect, DriverError, ExecutionId, FetchBudget, LexicalRules, Param,
    QueryLanguage, ResultItem, ScriptItem, ScriptItemKind, ScriptState, StatementClass,
    StatementSpan, TextPosition,
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
    /// Splitter state at [`Self::resume_from`] (the MySQL `DELIMITER` in force there), from
    /// [`RunEvent::NeedsConfirmation::script_state`]; `None`: rebuilt from the text before the
    /// offset with the session's current lexical modes.
    pub script_state: Option<ScriptState>,
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
            .field("script_state", &self.script_state)
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
            script_state: None,
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
    /// or released, or until it has waited longer than the idle limit with no fetch
    /// ([`RunEvent::ResultClosedIdle`], [`crate::ExecutionPreferences::paused_result_idle`]).
    /// In a script, every statement but the last is released after its first page
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

    /// The splitter state in force at [`Self::resume_from`]: pass
    /// [`RunEvent::NeedsConfirmation::script_state`] back with its `span.start`. Without it, the
    /// state is rebuilt by splitting the text before the offset (linear), which is exact unless
    /// a statement before it changed the lexical modes in a way that splits a `DELIMITER`
    /// directive differently.
    #[must_use]
    pub fn with_script_state(mut self, state: ScriptState) -> Self {
        self.script_state = Some(state);
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
    /// Items in the text from where the run started (statements, client directives and a
    /// refused piece), as split last: after a statement that changed the lexical modes, the
    /// rest of the text is split again.
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
    /// What each statement did, in order (F-SQL-16): at most [`MAX_STATEMENT_OUTCOMES`]; the
    /// rest is counted in [`Self::outcomes_dropped`].
    pub outcomes: Vec<StatementOutcome>,
    /// Statement outcomes not kept past [`MAX_STATEMENT_OUTCOMES`].
    pub outcomes_dropped: usize,
    /// The first statement that failed, timed out or was refused, if any.
    pub first_error: Option<FirstError>,
}

/// Statement outcomes kept in a [`RunSummary`].
pub const MAX_STATEMENT_OUTCOMES: usize = 10_000;

/// How one statement of a run ended ([`StatementOutcome::status`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum StatementStatus {
    /// Finished normally.
    Succeeded,
    /// Its result was closed early (released, idle, or a script moving on); counted as
    /// succeeded.
    Closed,
    /// Failed (engine error, refused piece, closed session).
    Failed,
    /// Cancelled by the user.
    Cancelled,
    /// Exceeded its timeout.
    TimedOut,
    /// Not sent: it needs the user's confirmation.
    NeedsConfirmation,
}

/// One statement of a run, for the run summary (F-SQL-16). Never holds statement text or
/// values.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct StatementOutcome {
    /// Item index ([`RunEvent`] indexes).
    pub index: usize,
    /// Its byte range in the editor buffer, when it was sent or asked for confirmation.
    pub span: Option<Range<usize>>,
    /// How it ended.
    pub status: StatementStatus,
    /// Rows / documents delivered.
    pub rows: u64,
    /// Sum of the update counts reported, if any.
    pub update_count: Option<u64>,
    /// Wall-clock time (zero when it was not sent).
    pub elapsed: Duration,
}

/// The first error of a run ([`RunSummary::first_error`]).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct FirstError {
    /// Item index.
    pub index: usize,
    /// The error, for display only (an engine message may quote values: never logged).
    pub message: String,
    /// Its short, stable kind ([`CoreError::kind`]), safe to log.
    pub kind: &'static str,
    /// Where it points, when the engine reports a position.
    pub position: Option<ErrorPosition>,
}

/// Statement outcomes of a run, collected from the events it sends.
#[derive(Default)]
struct RunLog {
    outcomes: Vec<StatementOutcome>,
    dropped: usize,
    first_error: Option<FirstError>,
    /// Statement index → (span, start) of statements sent and not ended yet.
    started: Option<(usize, Range<usize>, Instant)>,
}

impl RunLog {
    fn push(
        &mut self,
        index: usize,
        status: StatementStatus,
        rows: u64,
        update_count: Option<u64>,
        elapsed: Option<Duration>,
        span: Option<Range<usize>>,
    ) {
        let (span, elapsed) = match self.started.take() {
            Some((i, s, at)) if i == index => {
                (span.or(Some(s)), elapsed.unwrap_or_else(|| at.elapsed()))
            }
            other => {
                self.started = other;
                (span, elapsed.unwrap_or_default())
            }
        };
        if self.outcomes.len() >= MAX_STATEMENT_OUTCOMES {
            self.dropped += 1;
            return;
        }
        self.outcomes.push(StatementOutcome {
            index,
            span,
            status,
            rows,
            update_count,
            elapsed,
        });
    }

    fn observe(&mut self, event: &RunEvent) {
        match event {
            RunEvent::StatementStarted { index, span, .. } => {
                self.started = Some((*index, span.clone(), Instant::now()));
            }
            RunEvent::StatementFinished {
                index,
                rows,
                update_count,
                elapsed,
                ..
            } => self.push(
                *index,
                StatementStatus::Succeeded,
                *rows,
                *update_count,
                Some(*elapsed),
                None,
            ),
            RunEvent::StatementClosed { index, rows } => {
                self.push(*index, StatementStatus::Closed, *rows, None, None, None);
            }
            RunEvent::StatementFailed {
                index,
                error,
                position,
                elapsed,
            } => {
                self.push(
                    *index,
                    StatementStatus::Failed,
                    0,
                    None,
                    Some(*elapsed),
                    None,
                );
                if self.first_error.is_none() {
                    self.first_error = Some(FirstError {
                        index: *index,
                        message: error.to_string(),
                        kind: error.kind(),
                        position: *position,
                    });
                }
            }
            RunEvent::StatementCancelled { index, elapsed } => {
                self.push(
                    *index,
                    StatementStatus::Cancelled,
                    0,
                    None,
                    Some(*elapsed),
                    None,
                );
            }
            RunEvent::StatementTimedOut { index, after } => {
                self.push(
                    *index,
                    StatementStatus::TimedOut,
                    0,
                    None,
                    Some(*after),
                    None,
                );
                if self.first_error.is_none() {
                    self.first_error = Some(FirstError {
                        index: *index,
                        message: format!("the statement exceeded its timeout of {after:?}"),
                        kind: "timeout",
                        position: None,
                    });
                }
            }
            RunEvent::NeedsConfirmation { index, span, .. } => self.push(
                *index,
                StatementStatus::NeedsConfirmation,
                0,
                None,
                Some(Duration::ZERO),
                Some(span.clone()),
            ),
            _ => {}
        }
    }
}

/// Event of one run. Exactly one [`RunEvent::Done`] ends it. `index` is the 0-based index of
/// the item in the request text as the dialect splits it (client directives, never sent, keep
/// their index). Indexes are per run: after a statement that changed the session's lexical
/// modes the rest of the text is split again with the modes as they are then (review H2, P3-D
/// L2), so the indexes that follow come from that split. Resume by offset
/// ([`RunRequest::resume_from`]), never by index.
#[derive(Debug)]
#[non_exhaustive]
pub enum RunEvent {
    /// The run started: the text holds `statements` items (first event), as split with the
    /// lexical modes in force at the start.
    Started {
        /// Items in the text (statements and client directives).
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
    /// Statement `index` is not sent yet: it has waited more than 250 ms behind a metadata
    /// load of the same session (navigator, completion, a table description). At most once per
    /// statement; its timeout runs and "Cancel" ends the wait at once.
    WaitingForMetadata {
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
    /// Statement `index`'s paused result waited longer than the idle limit with no
    /// [`RunControl::fetch`] ([`crate::ExecutionPreferences::paused_result_idle`]), so the run
    /// released it before the driver's own limit could end the session. Sent just before the
    /// [`RunEvent::StatementClosed`] of that statement (only when the engine confirmed the
    /// close). The rows already delivered are kept; the session stays open and usable, and an
    /// open transaction stays open. Only reads pause, so only reads are closed this way.
    ResultClosedIdle {
        /// Statement index.
        index: usize,
        /// Rows / documents delivered before it was closed.
        rows: u64,
        /// How long it had been paused when the run released it.
        after: Duration,
    },
    /// Statement `index`'s stream was closed early: released ([`RunControl::release`]), closed
    /// after the idle limit ([`RunEvent::ResultClosedIdle`] comes first), or a script moved on
    /// to the next statement after its first page. Its result keeps the rows delivered.
    /// Counted as succeeded.
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
        /// Splitter state at `span.start` (the MySQL `DELIMITER` in force): pass it back with
        /// [`RunRequest::with_script_state`] when resuming.
        script_state: ScriptState,
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

/// Longest idle limit of a paused result, whatever the preferences hold: below the 300 s after
/// which the drivers end a session whose result is left waiting (pgwire `IDLE_CURSOR_TIMEOUT`,
/// mysqlwire `net_write_timeout`), with a margin for the release to reach the server.
pub const MAX_PAUSED_RESULT_IDLE: Duration =
    Duration::from_secs(crate::preferences::PAUSED_RESULT_IDLE_SECS_RANGE.1);

/// Shortest idle limit of a paused result, whatever the preferences hold (security review L1):
/// a zero or tiny value set in memory would close every paged read right after its first page.
pub const MIN_PAUSED_RESULT_IDLE: Duration =
    Duration::from_secs(crate::preferences::PAUSED_RESULT_IDLE_SECS_RANGE.0);

/// The idle limit of a paused result: the preference, clamped to
/// [`MIN_PAUSED_RESULT_IDLE`]..=[`MAX_PAUSED_RESULT_IDLE`], and the driver's own limit when it
/// declares a lower one (`driver`; it wins even below the floor, since past it the driver ends
/// the session). The driver API does not declare such a limit yet, so callers pass `None` until
/// it does.
#[must_use]
pub fn paused_result_idle_limit(preference: Duration, driver: Option<Duration>) -> Duration {
    let limit = preference.clamp(MIN_PAUSED_RESULT_IDLE, MAX_PAUSED_RESULT_IDLE);
    match driver {
        Some(d) => limit.min(d),
        None => limit,
    }
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
    /// Test hook: an idle limit used as is, below the floor ([`ExecutionService::set_paused_result_idle_for_tests`]).
    idle_override: RwLock<Option<Duration>>,
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
                idle_override: RwLock::new(None),
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

    /// **Tests only.** Uses `limit` as the idle limit of paused results of the next runs, as
    /// is (below [`MIN_PAUSED_RESULT_IDLE`] too), instead of the preference; `None` goes back
    /// to the preference. Never called by the application.
    #[doc(hidden)]
    pub fn set_paused_result_idle_for_tests(&self, limit: Option<Duration>) {
        *self
            .inner
            .idle_override
            .write()
            .unwrap_or_else(PoisonError::into_inner) = limit;
    }

    /// The query history, when it could be opened.
    #[must_use]
    pub fn history(&self) -> Option<&History> {
        self.inner.history.as_ref()
    }

    /// The items of `text` as the dialect of `engine` splits it for "execute script"
    /// ([`Dialect::split_script`]: statements, client directives such as a MySQL `DELIMITER`,
    /// refused pieces such as `COPY … FROM STDIN`), with their positions. Pure, never blocks.
    /// Uses the engine's default lexical modes (no session): [`SessionHandle::dialect`] gives
    /// the modes of an open session.
    ///
    /// # Errors
    /// [`CoreError::NoDriver`] when no driver serves `engine`.
    pub fn split_script(
        &self,
        engine: &EngineId,
        text: &str,
    ) -> Result<Vec<ScriptItem>, CoreError> {
        Ok(self
            .inner
            .manager
            .registry()
            .dialect(engine)?
            .split_script(text))
    }

    /// The statements of `text` as the dialect of `engine` splits it for "execute script"
    /// (for the editor: statement markers): the spans of the statement items of
    /// [`Self::split_script`], so a statement inside a MySQL `DELIMITER` section is one span
    /// and directives and refused pieces are left out. Pure, never blocks.
    ///
    /// # Errors
    /// [`CoreError::NoDriver`] when no driver serves `engine`.
    pub fn split(&self, engine: &EngineId, text: &str) -> Result<Vec<StatementSpan>, CoreError> {
        Ok(self
            .split_script(engine, text)?
            .into_iter()
            .filter(|i| i.kind == ScriptItemKind::Statement)
            .map(|i| i.span)
            .collect())
    }

    /// The statement at byte `cursor` of `text` ("Execute statement" with no selection): the
    /// item containing the cursor, else the closest one before it on the same or a previous
    /// line, else the next one. Client directives are never picked; a refused piece is (running
    /// it is refused, never something next to it). `None` when the text holds no statement.
    ///
    /// # Errors
    /// [`CoreError::NoDriver`].
    pub fn statement_at(
        &self,
        engine: &EngineId,
        text: &str,
        cursor: usize,
    ) -> Result<Option<StatementSpan>, CoreError> {
        let spans: Vec<StatementSpan> = self
            .split_script(engine, text)?
            .into_iter()
            .filter(|i| i.kind != ScriptItemKind::ClientDirective)
            .map(|i| i.span)
            .collect();
        Ok(pick_statement(&spans, cursor))
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
            progress: Progress::default(),
            idle_limit: self
                .inner
                .idle_override
                .read()
                .unwrap_or_else(PoisonError::into_inner)
                .unwrap_or_else(|| {
                    paused_result_idle_limit(prefs.execution.paused_result_idle, None)
                }),
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

/// Completes at `deadline` (never when `None`; the caller guards the branch).
async fn sleep_until_opt(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(d).await,
        None => std::future::pending().await,
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

/// Checks one item of a request before it can run (review N2): a refused piece (or a kind
/// this version does not know, fail-closed) refuses the run; "Execute statement" and bind
/// parameters need a single statement. `statements` counts the statement items seen so far.
fn check_item(
    item: &ScriptItem,
    kind: RunKind,
    has_params: bool,
    statements: &mut usize,
) -> Result<(), CoreError> {
    match item.kind {
        ScriptItemKind::Statement => {
            *statements += 1;
            if *statements > 1 && kind == RunKind::Statement {
                return Err(CoreError::InvalidRequest(
                    "the text holds several statements: run it as a script",
                ));
            }
            if *statements > 1 && has_params {
                return Err(CoreError::InvalidRequest(
                    "bind parameters need a single statement",
                ));
            }
            Ok(())
        }
        ScriptItemKind::ClientDirective => Ok(()),
        ScriptItemKind::Refused(why) => Err(CoreError::InvalidRequest(why.message())),
        _ => Err(CoreError::InvalidRequest(
            "the script holds a piece that cannot run as part of a script",
        )),
    }
}

/// An item the run refuses, found while splitting.
struct Refusal {
    /// Its item index.
    index: usize,
    /// Where it starts.
    at: TextPosition,
    error: CoreError,
}

/// One item taken from a [`ScriptCursor`].
struct Taken {
    index: usize,
    item: ScriptItem,
    /// Splitter state in force at the item's start.
    state: ScriptState,
}

/// The items of a script, split lazily and linearly as the P3-D contract asks
/// (`conformance::script_plan` is the reference loop): windows of
/// [`Dialect::split_script_ahead`] from the end of the last item, the [`ScriptState`] kept item
/// by item with [`Dialect::script_state_after`], every fetched item checked
/// ([`check_item`]), and a re-split from the end of the last item taken when the session's
/// lexical rules changed.
struct ScriptCursor<'a> {
    text: &'a str,
    dialect: &'a dyn Dialect,
    kind: RunKind,
    has_params: bool,
    /// Items split but not taken yet.
    pending: VecDeque<ScriptItem>,
    /// Where the next window starts, and the splitter state there.
    ahead: TextPosition,
    ahead_state: ScriptState,
    /// Nothing is left to split after `ahead` (or a refusal stopped the split).
    exhausted: bool,
    /// End of the last item taken, and the splitter state after it.
    at: TextPosition,
    state: ScriptState,
    /// Lexical rules the pending items were split with.
    rules: LexicalRules,
    /// Items taken, and statement items among them.
    taken: usize,
    taken_statements: usize,
    /// Statement items taken or pending.
    seen_statements: usize,
    /// The first refused item found (pending items stop before it).
    refusal: Option<Refusal>,
}

impl<'a> ScriptCursor<'a> {
    fn new(
        text: &'a str,
        dialect: &'a dyn Dialect,
        from: TextPosition,
        state: ScriptState,
        kind: RunKind,
        has_params: bool,
    ) -> Self {
        Self {
            text,
            dialect,
            kind,
            has_params,
            pending: VecDeque::new(),
            ahead: from,
            ahead_state: state.clone(),
            exhausted: false,
            at: from,
            state,
            rules: dialect.lexical_rules(),
            taken: 0,
            taken_statements: 0,
            seen_statements: 0,
            refusal: None,
        }
    }

    /// Splits the next window; `false` when nothing was added.
    fn fill(&mut self) -> bool {
        if self.exhausted || self.ahead.offset >= self.text.len() {
            self.exhausted = true;
            return false;
        }
        let more = self
            .dialect
            .split_script_ahead(self.text, self.ahead, &self.ahead_state);
        let mut added = false;
        for item in more {
            // Fail-safe: a splitter that does not move forward ends the split.
            if item.end.offset <= self.ahead.offset {
                break;
            }
            if let Err(error) =
                check_item(&item, self.kind, self.has_params, &mut self.seen_statements)
            {
                self.refusal = Some(Refusal {
                    index: self.taken + self.pending.len(),
                    at: item.start,
                    error,
                });
                self.exhausted = true;
                return added;
            }
            self.ahead_state = self
                .dialect
                .script_state_after(self.text, &item, &self.ahead_state);
            self.ahead = item.end;
            self.pending.push_back(item);
            added = true;
        }
        if !added {
            self.exhausted = true;
        }
        added
    }

    /// Splits everything left (one lazy pass).
    fn fill_all(&mut self) {
        while self.fill() {}
    }

    /// The next item, with the splitter state at its start.
    fn take(&mut self) -> Option<Taken> {
        if self.pending.is_empty() && !self.fill() {
            return None;
        }
        let item = self.pending.pop_front()?;
        let state = self.state.clone();
        self.state = self.dialect.script_state_after(self.text, &item, &state);
        self.at = item.end;
        let index = self.taken;
        self.taken += 1;
        if item.kind == ScriptItemKind::Statement {
            self.taken_statements += 1;
        }
        Some(Taken { index, item, state })
    }

    /// Whether anything but client directives is left: a statement, or a refusal (splits ahead
    /// as needed; amortised linear, since every window it splits is taken later).
    fn more_ahead(&mut self) -> bool {
        loop {
            if self
                .pending
                .iter()
                .any(|i| i.kind != ScriptItemKind::ClientDirective)
                || self.refusal.is_some()
            {
                return true;
            }
            if !self.fill() {
                return self.refusal.is_some();
            }
        }
    }

    /// When the session's lexical rules differ from the ones the pending items were split
    /// with, drops them and splits on from the end of the last item taken (P3-D L2). The new
    /// items are checked as they are split.
    fn resplit_if_rules_changed(&mut self) -> bool {
        let rules = self.dialect.lexical_rules();
        if rules == self.rules {
            return false;
        }
        self.rules = rules;
        self.pending.clear();
        self.ahead = self.at;
        self.ahead_state = self.state.clone();
        self.exhausted = false;
        self.seen_statements = self.taken_statements;
        // The first window is split and checked now (review N2).
        self.fill();
        true
    }

    /// Items taken plus everything left, as split last.
    fn total(&mut self) -> usize {
        self.fill_all();
        self.taken + self.pending.len() + usize::from(self.refusal.is_some())
    }
}

/// The splitter state at byte `from` of `text` (resuming without a carried state): the text
/// before it split lazily with `dialect`, every item that ends there applied. Linear.
fn script_state_at(dialect: &dyn Dialect, text: &str, from: usize) -> ScriptState {
    let mut state = ScriptState::default();
    let mut at = TextPosition::START;
    while at.offset < from {
        let items = dialect.split_script_ahead(text, at, &state);
        let mut moved = false;
        for item in &items {
            if item.span.start >= from || item.end.offset > from || item.end.offset <= at.offset {
                return state;
            }
            state = dialect.script_state_after(text, item, &state);
            at = item.end;
            moved = true;
        }
        if !moved {
            break;
        }
    }
    state
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

/// Error position of a 1-based character offset `char_pos` in the statement `item` of `text`,
/// counted from the item's own position (F-SQL-12): linear in the statement only.
fn error_position(text: &str, item: &ScriptItem, char_pos: u32, base: usize) -> ErrorPosition {
    let statement = item.text(text);
    let skip = usize::try_from(char_pos.saturating_sub(1)).unwrap_or(usize::MAX);
    let (mut line, mut column) = (item.start.line, item.start.column);
    let mut in_statement = statement.len();
    for (n, (i, c)) in statement.char_indices().enumerate() {
        if n == skip {
            in_statement = i;
            break;
        }
        if c == '\n' {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }
    ErrorPosition {
        offset: base + (item.span.start + in_statement).min(text.len()),
        line: u32::try_from(line).unwrap_or(u32::MAX),
        column: u32::try_from(column).unwrap_or(u32::MAX),
    }
}

/// The error position of a refused piece starting at `at`.
fn refusal_position(at: TextPosition, base: usize) -> ErrorPosition {
    ErrorPosition {
        offset: base + at.offset,
        line: u32::try_from(at.line).unwrap_or(u32::MAX),
        column: u32::try_from(at.column).unwrap_or(u32::MAX),
    }
}

/// Where the events of one run go: non-terminal events give up on cancel or when the UI is
/// gone; `Done` goes through a slot reserved at start.
struct RunOutput {
    events: mpsc::Sender<RunEvent>,
    cancel: CancelToken,
    terminal: Option<mpsc::OwnedPermit<RunEvent>>,
    /// [`RunEvent::Done`] was sent (or given up on because the UI is gone).
    finished: bool,
    /// Statement outcomes for the summary (F-SQL-16).
    log: std::sync::Mutex<RunLog>,
}

impl RunOutput {
    fn new(events: mpsc::Sender<RunEvent>, cancel: CancelToken) -> Self {
        let terminal = events.clone().try_reserve_owned().ok();
        Self {
            events,
            cancel,
            terminal,
            finished: false,
            log: std::sync::Mutex::new(RunLog::default()),
        }
    }

    fn observe(&self, event: &RunEvent) {
        self.log
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .observe(event);
    }

    /// A statement end sent without waiting (refusals before anything runs).
    fn try_send_end(&self, event: RunEvent) {
        self.observe(&event);
        let _ = self.events.try_send(event);
    }

    fn gone(&self) -> bool {
        self.events.is_closed()
    }

    /// `false` when not delivered (cancelled while the queue was full, or the UI is gone).
    async fn send(&self, event: RunEvent) -> bool {
        self.observe(&event);
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
            self.observe(&event);
            let _ = self.events.try_send(event);
        } else {
            let _ = self.send(event).await;
        }
    }

    fn done(&mut self, summary: RunSummary) {
        self.finished = true;
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
    /// Where the run is, for the terminal event of a run task that dies (see `Drop`).
    progress: Progress,
    /// A paused result waiting longer than this with no fetch is released (review M1).
    idle_limit: Duration,
}

/// How far a run went: the item being handled and the statement counts so far.
#[derive(Clone, Copy, Debug, Default)]
struct Progress {
    index: usize,
    executed: usize,
    succeeded: usize,
    failed: usize,
}

/// A run task that ends without its terminal event (a panic in a splitter or anywhere in the
/// task: the runtime drops the task's future after catching it) still sends exactly one
/// [`RunEvent::Done`], through the slot reserved at start (security review M1).
impl Drop for RunJob {
    fn drop(&mut self) {
        if self.out.finished {
            return;
        }
        tracing::error!("a run ended without its terminal event; reporting it as failed");
        let p = self.progress;
        let summary = self.summary(
            p.index + 1,
            (p.executed, p.succeeded, p.failed),
            RunEnd::Failed { index: p.index },
        );
        self.out.done(summary);
    }
}

/// Refuses a [`ScriptState`] the caller passed back ([`RunRequest::with_script_state`]) when
/// its delimiter is one the MySQL `DELIMITER` directive would refuse, or one no splitter can
/// scan: empty, with whitespace or a control character, a quote, a backslash, `#`, `--` or
/// `/*` (security review M1: an empty delimiter made a splitter panic).
fn check_script_state(state: &ScriptState) -> Result<(), CoreError> {
    let Some(delimiter) = &state.delimiter else {
        return Ok(());
    };
    let refused = delimiter.is_empty()
        || delimiter.chars().any(|c| {
            c.is_whitespace() || c.is_control() || matches!(c, '\\' | '\'' | '"' | '`' | '#')
        })
        || delimiter.contains("--")
        || delimiter.contains("/*");
    if refused {
        return Err(CoreError::InvalidRequest(
            "the script state's delimiter is not one a DELIMITER directive accepts",
        ));
    }
    Ok(())
}

/// One statement of a run, as [`RunJob::statement`] runs it.
struct Step {
    /// Item index.
    index: usize,
    /// The item, absolute in the request text.
    item: ScriptItem,
    /// Splitter state at its start (carried in [`RunEvent::NeedsConfirmation`]).
    script_state: ScriptState,
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
        let log = std::mem::take(&mut *self.out.log.lock().unwrap_or_else(PoisonError::into_inner));
        RunSummary {
            statements,
            executed: counts.0,
            succeeded: counts.1,
            failed: counts.2,
            end,
            elapsed: self.started.elapsed(),
            transaction: self.session.transaction_state(),
            outcomes: log.outcomes,
            outcomes_dropped: log.dropped,
            first_error: log.first_error,
        }
    }

    fn refuse(
        mut self,
        error: CoreError,
        index: usize,
        position: Option<ErrorPosition>,
        statements: usize,
    ) {
        self.out.try_send_end(RunEvent::StatementFailed {
            index,
            error,
            position,
            elapsed: Duration::ZERO,
        });
        let summary = self.summary(statements, (0, 0, 0), RunEnd::Refused);
        self.out.done(summary);
    }

    #[allow(clippy::too_many_lines)] // One loop over the script; split, it would be harder to follow.
    async fn run(
        mut self,
        request: RunRequest,
        dialect: Result<Arc<dyn Dialect>, CoreError>,
        prefs: &Preferences,
    ) {
        let dialect = match dialect {
            Ok(d) => d,
            Err(e) => return self.refuse(e, 0, None, 0),
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
                        None,
                        0,
                    );
                }
            },
        };
        if let Some(state) = &request.script_state
            && let Err(e) = check_script_state(state)
        {
            return self.refuse(e, 0, None, 0);
        }
        // The splitter state there: carried from the confirmation, else rebuilt from the text
        // before it (linear).
        let state = match (&request.script_state, request.resume_from) {
            (Some(state), Some(_)) => state.clone(),
            (_, None) => ScriptState::default(),
            (None, Some(_)) if from == 0 => ScriptState::default(),
            (None, Some(_)) => script_state_at(&*dialect, &request.text, from),
        };
        let start = if from == 0 {
            TextPosition::START
        } else {
            ninesql_driver_api::position_at(&request.text, from)
        };
        let mut cursor = ScriptCursor::new(
            &request.text,
            &*dialect,
            start,
            state,
            request.kind,
            !request.params.is_empty(),
        );
        // One lazy pass up front: a refused piece refuses the whole script before anything
        // runs (and "Execute statement" / bind parameters need a single statement).
        cursor.fill_all();
        if let Some(refusal) = cursor.refusal.take() {
            let total = cursor.pending.len() + 1;
            let position = Some(refusal_position(refusal.at, request.offset));
            return self.refuse(refusal.error, refusal.index, position, total);
        }
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
                statements: cursor.pending.len(),
            })
            .await;
        let (mut executed, mut succeeded, mut failed) = (0usize, 0usize, 0usize);
        let mut end = RunEnd::Completed;
        let mut first_statement = true;
        // The refused piece reported, counted in the summary.
        let mut refused = 0;
        loop {
            // A refused piece found by a re-split (or by looking ahead) stops the run before
            // anything more is sent (review N2).
            if let Some(refusal) = cursor.refusal.take() {
                self.out.try_send_end(RunEvent::StatementFailed {
                    index: refusal.index,
                    error: refusal.error,
                    position: Some(refusal_position(refusal.at, request.offset)),
                    elapsed: Duration::ZERO,
                });
                refused = 1;
                end = RunEnd::Refused;
                break;
            }
            let Some(Taken { index, item, state }) = cursor.take() else {
                // Splitting the next window may have found a refused piece (a re-split whose
                // first window held only directives): report it (security review L1).
                if cursor.refusal.is_some() {
                    continue;
                }
                break;
            };
            self.progress.index = index;
            if item.kind != ScriptItemKind::Statement {
                // Client directives are applied by the splitter, never sent.
                continue;
            }
            if self.cancel.is_cancelled() || self.out.gone() {
                end = RunEnd::Cancelled { index };
                break;
            }
            // Whether it is the last statement (a paged result stays open). Looking ahead may
            // find a refused piece: then nothing more runs.
            let last = !cursor.more_ahead();
            if cursor.refusal.is_some() {
                continue;
            }
            let text = item.text(&request.text).to_owned();
            let mut exec_request = ExecuteRequest::sql(text.clone())
                .with_language(request.language)
                .with_params(request.params.clone())
                .with_timeout(timeout)
                .with_budget(budget);
            // The token goes to the first statement of the run only, and only when it starts
            // exactly where the run was asked to resume.
            if first_statement
                && let Some(token) = request.confirmation
                && (request.resume_from.is_none() || item.span.start == from)
            {
                exec_request = exec_request.confirmed(token);
            }
            first_statement = false;
            // Classified now, with the lexical modes in force now (review H2); the session task
            // classifies it again for the confirmation, at the moment it sends it. Only reads
            // may be paused and released: a release is a server-side cancel, which would roll
            // back a write (`DELETE … RETURNING`, `CALL`, DML in a transaction).
            let class = dialect.classify(&text);
            let is_read = class == StatementClass::Read;
            let step = Step {
                index,
                item,
                script_state: state,
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
            self.progress.executed = executed;
            self.progress.succeeded = succeeded;
            self.progress.failed = failed;
            // After every statement (P3-D L2): when the session's lexical rules changed (a
            // `SET`, a `ROLLBACK` undoing one, a function calling `set_config`…), the rest is
            // split again from the end of this statement with the rules as they are now.
            // Offsets stay absolute; the new items are checked as they are split.
            cursor.resplit_if_rules_changed();
        }
        let statements = cursor.total() + refused;
        let summary = self.summary(statements, (executed, succeeded, failed), end);
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
            item,
            script_state,
            last,
            is_read,
        } = step;
        let span = item.span;
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
        // When the result paused (no fetch since), and how long it had been paused when the run
        // released it after the idle limit (review M1).
        let mut paused_since: Option<tokio::time::Instant> = None;
        let mut idle_closed: Option<Duration> = None;
        let idle_limit = self.idle_limit;
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
            if !paused {
                paused_since = None;
            }
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
            // The idle clock starts once the page end was handed to the UI (or given up on) and
            // the result is still paused; a fetch that brings more rows stops it.
            let idle_deadline = if paused && is_read {
                Some(*paused_since.get_or_insert_with(tokio::time::Instant::now) + idle_limit)
            } else {
                None
            };
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
                // Paused with no fetch for too long: released as `release` would (reads only),
                // before the driver's own limit ends the session (review M1).
                () = sleep_until_opt(idle_deadline), if idle_deadline.is_some() => {
                    if let Some(since) = paused_since {
                        idle_closed = Some(since.elapsed());
                    }
                    tracing::debug!(index, "paused result idle past its limit; releasing it");
                    execution.cancel();
                    releasing = true;
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
                ExecEvent::WaitingForMetadata => {
                    if forwarding {
                        forwarding = self.out.send(RunEvent::WaitingForMetadata { index }).await;
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
                            script_state,
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
                            // Not after a cancel: `send_end` then only `try_send`s, and this
                            // event must never take the slot of the `StatementClosed` (security
                            // review L3).
                            if let Some(after) = idle_closed
                                && !self.cancel.is_cancelled()
                            {
                                self.out
                                    .send_end(RunEvent::ResultClosedIdle {
                                        index,
                                        rows: delivered,
                                        after,
                                    })
                                    .await;
                            }
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
                                }) => Some(error_position(&request.text, &item, *p, base)),
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

    fn texts<'a>(cursor: &mut ScriptCursor<'_>, text: &'a str) -> Vec<&'a str> {
        let mut out = Vec::new();
        while let Some(t) = cursor.take() {
            out.push(t.item.text(text));
        }
        out
    }

    /// Review H2: the rest of a script is split lazily from a position, spans stay absolute.
    #[test]
    fn cursor_splits_from_a_position_with_absolute_offsets() {
        let d = ninesql_driver_api::AnsiDialect;
        let text = "SET x = 1;\nSELECT 1;\nSELECT 2";
        let mut all = ScriptCursor::new(
            text,
            &d,
            TextPosition::START,
            ScriptState::default(),
            RunKind::Script,
            false,
        );
        assert_eq!(texts(&mut all, text), ["SET x = 1", "SELECT 1", "SELECT 2"]);
        let from = ninesql_driver_api::position_at(text, 10);
        let mut rest = ScriptCursor::new(
            text,
            &d,
            from,
            ScriptState::default(),
            RunKind::Script,
            false,
        );
        rest.fill_all();
        assert_eq!(rest.total(), 2);
        assert_eq!(texts(&mut rest, text), ["SELECT 1", "SELECT 2"]);
        let end = ninesql_driver_api::position_at(text, text.len());
        let mut none = ScriptCursor::new(
            text,
            &d,
            end,
            ScriptState::default(),
            RunKind::Script,
            false,
        );
        assert!(none.take().is_none());
        assert!(!none.more_ahead());
    }

    #[test]
    fn items_are_checked_before_anything_runs() {
        let d = ninesql_driver_api::AnsiDialect;
        let items = d.split_script("SELECT 1; SELECT 2");
        let mut n = 0;
        assert!(check_item(&items[0], RunKind::Statement, true, &mut n).is_ok());
        assert!(check_item(&items[1], RunKind::Statement, false, &mut n).is_err());
        let mut n = 0;
        check_item(&items[0], RunKind::Script, true, &mut n).unwrap();
        assert!(check_item(&items[1], RunKind::Script, true, &mut n).is_err());
        let mut n = 0;
        check_item(&items[0], RunKind::Script, false, &mut n).unwrap();
        check_item(&items[1], RunKind::Script, false, &mut n).unwrap();
        assert_eq!(n, 2);
        // A "single statement" request that splits into two is refused up front, at the
        // second statement.
        let text = "SELECT 1; SELECT 2";
        let mut c = ScriptCursor::new(
            text,
            &d,
            TextPosition::START,
            ScriptState::default(),
            RunKind::Statement,
            false,
        );
        c.fill_all();
        let refusal = c.refusal.as_ref().unwrap();
        assert_eq!((refusal.index, refusal.at.offset), (1, 10));
        assert_eq!(c.pending.len(), 1);
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
        let d = ninesql_driver_api::AnsiDialect;
        let text = "SELECT 1;\nSELECT é,\n  nope FROM t";
        let items = d.split_script(text);
        let s = &items[1];
        assert_eq!(s.span.start, 10);
        // 1-based char 13 of the statement is `n` of `nope` (`SELECT é,\n  ` is 12 chars).
        let p = error_position(text, s, 13, 100);
        assert_eq!(&text[p.offset - 100..p.offset + 4 - 100], "nope");
        assert_eq!((p.line, p.column), (3, 3));
        // Out of range: the end of the statement.
        let p = error_position(text, s, 10_000, 0);
        assert_eq!(p.offset, text.len());
        assert_eq!((p.line, p.column), (3, 14));
        let p = error_position(text, &items[0], 1, 0);
        assert_eq!((p.offset, p.line, p.column), (0, 1, 1));
    }

    #[test]
    fn the_script_state_at_an_offset_is_rebuilt_from_the_prefix() {
        let d = ninesql_driver_api::AnsiDialect;
        let text = "SELECT 1;\nSELECT 2;\nSELECT 3";
        // ANSI has no directive: the state stays the default whatever the offset.
        for from in [0, 10, 11, 20, text.len()] {
            assert_eq!(script_state_at(&d, text, from), ScriptState::default());
        }
    }
}
