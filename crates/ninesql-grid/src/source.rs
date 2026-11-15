//! The adapter between the grid and whoever runs the statements (the core's execution service,
//! P3-C).
//!
//! The grid never touches a session or a [`ResultStream`](ninesql_core::driver_api::ResultStream)
//! and never awaits anything (I6): the execution service polls the stream on the driver runtime
//! and hands the grid [`ExecutionEvent`]s on the foreground thread; the grid asks for more rows
//! through a [`ResultSource`], whose methods must return at once.
//!
//! # Contract
//! - One [`ExecutionEvent::Started`] begins an execution; the grid clears its previous results.
//! - Statements are numbered by the execution (`statement`, 0-based, script order). Every
//!   [`ResultItem`] of a statement's stream is forwarded as [`ExecutionEvent::Item`], in stream
//!   order, `ResultItem::Truncated` included.
//! - **First page**: after a result-set header, the service delivers the first page (at least
//!   [`GridBudget::page_rows`](crate::GridBudget::page_rows) rows, or the end) without being
//!   asked, so the first rows show without a round trip through the grid. It then stops polling
//!   (back-pressure pauses the server cursor).
//! - **More rows**: [`ResultSource::fetch`] asks for at least `rows` more rows of the
//!   statement's current result set ([`FetchMode::Page`]) or for everything up to the end or the
//!   budget ([`FetchMode::All`]). The service ignores a request for a statement whose stream is
//!   gone.
//! - **End**: [`ExecutionEvent::StatementFinished`] when the stream returned `None` (complete or
//!   after `Truncated`); [`ExecutionEvent::StatementClosed`] when the service dropped the stream
//!   before its end (the next statement of a script needed the session, the tab was closed…):
//!   rows not fetched by then are not reachable without running the statement again.
//! - [`ResultSource::release`] tells the service the grid will take no more rows of that
//!   statement (the grid's memory budget is reached): it drops the stream, which releases the
//!   server cursor, and answers with `StatementClosed`.
//! - Errors carry a user-presentable message (`DriverError`'s `Display`): never a secret or a
//!   value. Notice messages are display-only: the grid never logs them.

use std::fmt;

use ninesql_core::driver_api::ResultItem;

/// How many rows a fetch asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchMode {
    /// At least this many more rows (or the end of the result set).
    Page {
        /// Rows asked for.
        rows: u32,
    },
    /// Every remaining row, bounded by the result's
    /// [`FetchBudget`](ninesql_core::driver_api::FetchBudget): the stream ends with
    /// `Truncated` at the budget, never loads an unbounded result (I6).
    All,
}

/// A request for more rows of one statement's current result set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FetchRequest {
    /// Statement number within the execution.
    pub statement: u32,
    /// Result-set index within the statement (the header's `index`).
    pub result_set: u32,
    /// How many rows.
    pub mode: FetchMode,
}

/// Implemented by the execution service (or a test double). Every method returns at once: the
/// work happens on the driver runtime and comes back as [`ExecutionEvent`]s.
pub trait ResultSource {
    /// Asks for more rows.
    fn fetch(&self, request: FetchRequest);
    /// The grid takes no more rows of `statement`: drop its stream.
    fn release(&self, statement: u32);
}

/// What the execution service reports, in order, for one execution.
#[derive(Clone)]
#[non_exhaustive]
pub enum ExecutionEvent {
    /// A new execution starts; previous results are cleared.
    Started,
    /// A statement starts running.
    StatementStarted {
        /// Statement number.
        statement: u32,
    },
    /// One item of the statement's result stream.
    Item {
        /// Statement number.
        statement: u32,
        /// The item, as the [`ResultStream`](ninesql_core::driver_api::ResultStream) yielded it.
        item: ResultItem,
    },
    /// The statement's stream ended (`None`): the result is complete, or ended with `Truncated`.
    StatementFinished {
        /// Statement number.
        statement: u32,
    },
    /// The service dropped the statement's stream before its end.
    StatementClosed {
        /// Statement number.
        statement: u32,
    },
    /// The statement failed. The message is user-presentable and carries no secret or value.
    StatementFailed {
        /// Statement number.
        statement: u32,
        /// Error text.
        message: String,
    },
    /// The execution was cancelled by the user.
    Cancelled,
    /// The execution ended (every statement ran, or the script stopped).
    Finished,
}

impl fmt::Debug for ExecutionEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never prints values or messages (they may quote data).
        match self {
            Self::Started => f.write_str("Started"),
            Self::StatementStarted { statement } => write!(f, "StatementStarted({statement})"),
            Self::Item { statement, item } => write!(f, "Item({statement}, {item:?})"),
            Self::StatementFinished { statement } => write!(f, "StatementFinished({statement})"),
            Self::StatementClosed { statement } => write!(f, "StatementClosed({statement})"),
            Self::StatementFailed { statement, .. } => write!(f, "StatementFailed({statement})"),
            Self::Cancelled => f.write_str("Cancelled"),
            Self::Finished => f.write_str("Finished"),
        }
    }
}
