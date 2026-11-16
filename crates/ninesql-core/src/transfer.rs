//! Transfer service (P5-C): streamed export of a statement's or a table's rows to a file
//! (F-XFER-01) and CSV import into an existing table (F-XFER-03), on a [`SessionHandle`], off the
//! UI thread, with bounded memory, progress, cancel and a summary. The formats, the export file
//! and the CSV reader are in [`ninesql_transfer`].
//!
//! # Using it (UI)
//! ```text
//! let job = core.transfer().export(&session, ExportRequest::table(path_parts, file, options));
//! loop { match job.next().await { Some(ExportEvent::Done(summary)) => …, Some(ExportEvent::Failed(e)) => …, … } }
//! job.cancel();   // removes the partial file
//!
//! let preview = core.transfer().preview_csv(CsvPreviewRequest::new(file, targets)).await;
//! let job = core.transfer().import_csv(&session, ImportRequest::new(file, table, targets, mapping));
//! // ImportEvent::NeedsConfirmation(c) on production / confirm-every-write types: show c, then
//! // import_csv again with the same request and ImportRequest::confirmed(c.token).
//! ```
//!
//! # Export
//! - **Reads only.** The statement (or `SELECT * FROM <table>`, quoted by the session's
//!   dialect) is classified with the session's dialect and must be a
//!   [`StatementClass::Read`]: an export runs the statement again, so a write is never run by an
//!   export ([`TransferJobError::NotARead`]). When the session has no open transaction and is not
//!   read-only already, the export runs inside an engine-side read-only transaction (PostgreSQL
//!   family `BEGIN READ ONLY`, MySQL family `START TRANSACTION READ ONLY`, rolled back at the end;
//!   SQLite and engines without transactions: none), so a function with side effects that the
//!   classifier reads as a read is refused by the engine. If it cannot be opened, the export
//!   fails on production and confirm-every-write connections and goes on without it elsewhere.
//!   The statements go through the session task, so the session's read-only mode and safety
//!   rules still apply.
//! - **Exclusive session.** An export and an import lease their session for their whole run:
//!   meanwhile every other statement or transaction command on it fails with
//!   [`CoreError::SessionLeased`], and a session that is running or queuing a statement (a
//!   paused grid result included) refuses the transfer with [`TransferJobError::SessionBusy`].
//!   The UI may still prefer a dedicated session so the editor stays usable.
//! - **Memory.** The result is fetched in batches under the request's [`FetchBudget`] with no
//!   overall byte or row limit (the rows are written, not kept), and each batch goes to a writer
//!   on the blocking pool through a queue of [`WRITER_QUEUE`] batches; the exporter keeps one row
//!   of text and the file a 64 KiB buffer. What the export holds is at most
//!   `(WRITER_QUEUE + 2) × max_batch_bytes` plus the session's own result queue;
//!   [`ExportSummary::peak_buffered_bytes`] reports the measured peak of the batches it held.
//! - **The file (I1, I4).** [`TransferService::check_export_path`] resolves the directory
//!   (symbolic links included) and refuses a path inside a DBeaver workspace ([`DbeaverRoots`],
//!   fail-closed) or inside NineSQL's own configuration, data or state directories (secret
//!   vault, `connections.json`, history). [`OutputFile`] then writes a new `0600` file next to
//!   the target and moves it into place at the end, never through a symbolic link and never
//!   over an existing file unless the request says [`Overwrite::Confirmed`]; on cancel or error
//!   the partial file is removed. No secret is ever written: only result values are.
//!
//! # Import
//! - **Refusals before anything is sent:** a read-only session (I3), a driver without bind
//!   parameters (I8), a session with an open transaction (the import would mix into it), an
//!   all-or-nothing import on an engine without transactions, an invalid mapping, a file that is
//!   not a regular file or is over [`CsvLimits::max_file_bytes`], a file inside NineSQL's own
//!   storage.
//! - **Confirmation (ADR-0007).** An import is a write. On a connection typed production, or
//!   whose type confirms every write, the first call ends with
//!   [`ImportEvent::NeedsConfirmation`] and a single-use [`ImportToken`] bound to the session,
//!   the file, the table and the mapped columns (ten minutes). The UI shows it and calls
//!   [`TransferService::import_csv`] again with [`ImportRequest::confirmed`]. Once confirmed,
//!   the per-statement confirmation the session asks for each `INSERT` (types that confirm every
//!   write) is answered by the service for the confirmed statement shape only: a statement whose
//!   class is not [`StatementClass::Write`] or whose text is not the import's `INSERT` fails the
//!   import. (ADR candidate: one confirmation for a bulk write.)
//! - **Statements.** Rows are sent as parameterized multi-row `INSERT`s (at most
//!   [`MAX_ROWS_PER_INSERT`] rows and [`MAX_PARAMS_PER_INSERT`] parameters each), built by an
//!   [`InsertRenderer`] (the P5-B integration point, [`ninesql_transfer::sql`]). Values are
//!   converted to typed parameters from the target column types; nothing is spliced into SQL
//!   text.
//! - **Transactions.** [`ImportTransaction::AllOrNothing`] (default) runs the whole import in
//!   one transaction, rolled back on any error or cancel; [`ImportTransaction::CommitEvery`]
//!   commits every N rows (rows committed before an error stay). Engines without transactions
//!   support only `CommitEvery` (each statement commits on its own).
//! - **Invalid rows.** A row that cannot be read or converted stops the import, unless
//!   [`ImportRequest::skip_invalid_rows`]: it is then skipped and reported. The summary keeps the
//!   first [`MAX_REPORTED_ISSUES`] issues (line, column, reason; never a value) and the total.
//! - **Memory.** The file is read on the blocking pool one statement's worth of rows at a time
//!   (at most [`MAX_INSERT_BYTES`] of parameters), through a one-slot queue.
//! - Nothing of the file is logged; the per-statement query text rules of the session apply.

use std::fmt;
use std::fs;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use ninesql_driver_api::{CancelToken, FetchBudget, Param, ResultItem, StatementClass};
use ninesql_storage::Paths;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::dbeaver_roots::DbeaverRoots;
// The formats, options, CSV and mapping types, so the UI needs only `ninesql_core::transfer`.
use crate::error::CoreError;
use crate::session::{
    ExecEvent, ExecuteRequest, Next, Reply, SessionHandle, SessionLease, SessionManager,
};
pub use ninesql_transfer::*;

/// Batches queued between the session and the export writer.
pub const WRITER_QUEUE: usize = 2;
/// Most rows in one import `INSERT`.
pub const MAX_ROWS_PER_INSERT: usize = 1000;
/// Most bind parameters in one import `INSERT` (below SQLite's default limit of 32 766).
pub const MAX_PARAMS_PER_INSERT: usize = 30_000;
/// Most parameter bytes in one import `INSERT`.
pub const MAX_INSERT_BYTES: usize = 4 * 1024 * 1024;
/// Issues kept in an import summary.
pub const MAX_REPORTED_ISSUES: usize = 100;
/// Why NineSQL's own directories are refused.
const NINESQL_STORAGE: &str =
    "the folder is part of NineSQL's own storage (workspace, secret vault, history)";
/// Why `~/.ssh` is refused.
const SSH_DIR: &str = "the folder holds SSH keys and settings";
/// Events buffered per job before progress events are dropped.
const EVENT_QUEUE: usize = 16;
/// Lifetime of an import confirmation.
const CONFIRMATION_TTL: Duration = Duration::from_secs(10 * 60);
/// Import confirmations kept.
const MAX_PENDING_CONFIRMATIONS: usize = 16;
/// Least time between two progress events.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(200);

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Why a transfer failed or was refused. Messages never hold a value from the data or the file.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TransferJobError {
    /// A file, format or CSV error.
    #[error(transparent)]
    Transfer(#[from] TransferError),
    /// A session or driver error.
    #[error(transparent)]
    Core(#[from] CoreError),
    /// An export runs its statement again: only reads are exported.
    #[error("only a query that reads data can be exported (this statement is classified {0:?})")]
    NotARead(StatementClass),
    /// An import is a write (I3).
    #[error("the session is read-only: an import writes data")]
    ReadOnlySession,
    /// The driver does not support bind parameters (I8).
    #[error("this connection does not support bind parameters, which the import needs")]
    NoParameters,
    /// An all-or-nothing import needs transactions.
    #[error("this connection has no transactions: choose \"commit every N rows\"")]
    NoTransactions,
    /// A transaction is open on the session.
    #[error("a transaction is open on this session: commit or roll back before importing")]
    PendingTransaction,
    /// A row could not be read or converted (and invalid rows are not skipped).
    #[error("invalid row: {0}")]
    InvalidRow(String),
    /// The session runs or queues other work, or another transfer holds it.
    #[error("the session is busy: wait for its statement to end, or use another session")]
    SessionBusy,
    /// The confirmation does not match the import or expired.
    #[error("the confirmation does not match this import, or expired; confirm it again")]
    InvalidConfirmation,
    /// A statement needed a confirmation the import cannot answer.
    #[error("the database session asked to confirm a statement the import did not expect")]
    UnexpectedConfirmation,
    /// The transfer was cancelled.
    #[error("cancelled")]
    Cancelled,
}

impl TransferJobError {
    /// A short, stable kind, safe to log.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Transfer(TransferError::Cancelled) | Self::Cancelled => "cancelled",
            Self::Transfer(_) => "transfer",
            Self::Core(e) => e.kind(),
            Self::NotARead(_) => "not-a-read",
            Self::ReadOnlySession => "read-only-session",
            Self::NoParameters => "no-parameters",
            Self::NoTransactions => "no-transactions",
            Self::PendingTransaction => "pending-transaction",
            Self::InvalidRow(_) => "invalid-row",
            Self::SessionBusy => "session-busy",
            Self::InvalidConfirmation => "invalid-confirmation",
            Self::UnexpectedConfirmation => "unexpected-confirmation",
        }
    }

    fn from_core(e: CoreError) -> Self {
        match e {
            CoreError::Driver(ninesql_driver_api::DriverError::Cancelled) => Self::Cancelled,
            other => Self::Core(other),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Jobs

/// A running transfer's events and cancel path. Dropping it cancels the transfer.
pub struct Job<E> {
    events: mpsc::Receiver<E>,
    cancel: CancelToken,
}

impl<E> fmt::Debug for Job<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Job")
            .field("cancelled", &self.cancel.is_cancelled())
            .finish_non_exhaustive()
    }
}

impl<E> Job<E> {
    /// Stops the transfer: the running statement is cancelled, an export's partial file is
    /// removed, an import's transaction is rolled back. Never blocks; idempotent.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// Next event; `None` after the terminal one.
    pub async fn next(&mut self) -> Option<E> {
        self.events.recv().await
    }

    /// Next event without waiting.
    pub fn try_next(&mut self) -> Next<E> {
        match self.events.try_recv() {
            Ok(e) => Next::Ready(e),
            Err(mpsc::error::TryRecvError::Empty) => Next::Pending,
            Err(mpsc::error::TryRecvError::Disconnected) => Next::Ended,
        }
    }
}

impl<E> Drop for Job<E> {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// A running export.
pub type ExportJob = Job<ExportEvent>;
/// A running import.
pub type ImportJob = Job<ImportEvent>;

// ---------------------------------------------------------------------------------------------
// Export model

/// What to export.
#[derive(Clone, PartialEq)]
#[non_exhaustive]
pub enum ExportSource {
    /// The rows of one query (run again for the export), with its bind parameters.
    Statement {
        /// One statement; it must be a read.
        text: String,
        /// Bind parameters (never logged).
        params: Vec<Param>,
    },
    /// Every row of a table or view: `SELECT * FROM <name>`, quoted by the session's dialect.
    Table {
        /// Qualified name parts (`["public", "orders"]`).
        path: Vec<String>,
    },
}

impl fmt::Debug for ExportSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Statement { text, params } => f
                .debug_struct("Statement")
                .field("text_len", &text.len())
                .field("params", &params.len())
                .finish(),
            Self::Table { path } => f.debug_struct("Table").field("parts", &path.len()).finish(),
        }
    }
}

/// An export request. Build with [`ExportRequest::statement`] or [`ExportRequest::table`].
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct ExportRequest {
    /// What to export.
    pub source: ExportSource,
    /// The file the user chose (absolute).
    pub path: PathBuf,
    /// Replace an existing file only when the user confirmed it.
    pub overwrite: Overwrite,
    /// Format and options. For [`ExportFormat::SqlInsert`] of a table, an empty
    /// `options.sql.table` defaults to the table's name.
    pub options: ExportOptions,
    /// Wall-clock limit of the statement, fetch included (`None`: none).
    pub timeout: Option<Duration>,
    /// Batch size and per-value limits of the fetch; its overall `max_bytes` / `max_rows` are
    /// ignored (the rows are written, not kept).
    pub budget: FetchBudget,
}

impl ExportRequest {
    /// Exports the rows of a query.
    #[must_use]
    pub fn statement(
        text: impl Into<String>,
        path: impl Into<PathBuf>,
        options: ExportOptions,
    ) -> Self {
        Self::new(
            ExportSource::Statement {
                text: text.into(),
                params: Vec::new(),
            },
            path.into(),
            options,
        )
    }

    /// Exports a whole table or view.
    #[must_use]
    pub fn table(table: Vec<String>, path: impl Into<PathBuf>, options: ExportOptions) -> Self {
        Self::new(ExportSource::Table { path: table }, path.into(), options)
    }

    fn new(source: ExportSource, path: PathBuf, options: ExportOptions) -> Self {
        Self {
            source,
            path,
            overwrite: Overwrite::Refuse,
            options,
            timeout: None,
            budget: FetchBudget::default().with_batch_rows(1000),
        }
    }

    /// Sets bind parameters of a statement source.
    #[must_use]
    pub fn with_params(mut self, values: Vec<Param>) -> Self {
        if let ExportSource::Statement { params, .. } = &mut self.source {
            *params = values;
        }
        self
    }

    /// Allows replacing an existing file (the user confirmed it).
    #[must_use]
    pub fn overwrite_confirmed(mut self) -> Self {
        self.overwrite = Overwrite::Confirmed;
        self
    }

    /// Sets the timeout.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.timeout = timeout;
        self
    }

    /// Sets the fetch budget.
    #[must_use]
    pub fn with_budget(mut self, budget: FetchBudget) -> Self {
        self.budget = budget;
        self
    }
}

/// Summary of a finished export.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ExportSummary {
    /// The file written (directory resolved).
    pub path: PathBuf,
    /// Rows written.
    pub rows: u64,
    /// Bytes written.
    pub bytes: u64,
    /// Wall-clock time.
    pub elapsed: Duration,
    /// Columns in which the driver cut at least one value (the file has the kept prefix).
    pub truncated_value_columns: Vec<String>,
    /// Further result sets, not exported.
    pub result_sets_skipped: u32,
    /// The driver stopped the result early (budget): the file holds the rows delivered before.
    pub incomplete: bool,
    /// Peak bytes of result batches held by the export at once (measured).
    pub peak_buffered_bytes: u64,
}

/// Event of an export. Exactly one of [`ExportEvent::Done`] or [`ExportEvent::Failed`] ends it.
#[derive(Debug)]
#[non_exhaustive]
pub enum ExportEvent {
    /// The statement was sent (the file is being created next to the target).
    Started,
    /// Progress (dropped when the UI reads slowly).
    Progress {
        /// Rows received so far.
        rows: u64,
        /// Bytes written so far.
        bytes: u64,
    },
    /// The file is complete and in place (terminal).
    Done(ExportSummary),
    /// The export failed or was cancelled; no file was left behind (terminal).
    Failed(TransferJobError),
}

// ---------------------------------------------------------------------------------------------
// Import model

/// How an import uses transactions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum ImportTransaction {
    /// One transaction for the whole file, rolled back on any error or cancel (default).
    #[default]
    AllOrNothing,
    /// A commit every N rows (at least 1); rows committed before an error stay.
    CommitEvery(u64),
}

/// Single-use permission to run one import ([`ImportEvent::NeedsConfirmation`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ImportToken(Uuid);

/// A CSV import request. Build with [`ImportRequest::new`].
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct ImportRequest {
    /// The CSV file.
    pub path: PathBuf,
    /// Target table (qualified name parts).
    pub table: Vec<String>,
    /// Columns of the target table (from the metadata service).
    pub targets: Vec<TargetColumn>,
    /// File fields to target columns.
    pub mapping: ColumnMapping,
    /// CSV syntax.
    pub format: CsvFormat,
    /// Size caps.
    pub limits: CsvLimits,
    /// NULL text, empty fields, trimming.
    pub values: ValueOptions,
    /// Transactions.
    pub transaction: ImportTransaction,
    /// Skip and report invalid rows instead of stopping.
    pub skip_invalid_rows: bool,
    /// Rows per `INSERT` (1 to [`MAX_ROWS_PER_INSERT`], default 500; fewer when the columns
    /// would exceed [`MAX_PARAMS_PER_INSERT`]).
    pub rows_per_insert: usize,
    /// Wall-clock limit of each statement (`None`: none).
    pub statement_timeout: Option<Duration>,
    /// The user's confirmation, when the first call asked for one.
    pub confirmation: Option<ImportToken>,
}

impl ImportRequest {
    /// An all-or-nothing import with the default CSV syntax and limits.
    #[must_use]
    pub fn new(
        path: impl Into<PathBuf>,
        table: Vec<String>,
        targets: Vec<TargetColumn>,
        mapping: ColumnMapping,
    ) -> Self {
        Self {
            path: path.into(),
            table,
            targets,
            mapping,
            format: CsvFormat::default(),
            limits: CsvLimits::default(),
            values: ValueOptions::default(),
            transaction: ImportTransaction::AllOrNothing,
            skip_invalid_rows: false,
            rows_per_insert: 500,
            statement_timeout: None,
            confirmation: None,
        }
    }

    /// Sets the CSV syntax.
    #[must_use]
    pub fn with_format(mut self, format: CsvFormat) -> Self {
        self.format = format;
        self
    }

    /// Sets the limits.
    #[must_use]
    pub fn with_limits(mut self, limits: CsvLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Sets the value options.
    #[must_use]
    pub fn with_values(mut self, values: ValueOptions) -> Self {
        self.values = values;
        self
    }

    /// Sets the transaction mode.
    #[must_use]
    pub fn with_transaction(mut self, transaction: ImportTransaction) -> Self {
        self.transaction = transaction;
        self
    }

    /// Skips and reports invalid rows.
    #[must_use]
    pub fn skipping_invalid_rows(mut self, skip: bool) -> Self {
        self.skip_invalid_rows = skip;
        self
    }

    /// Sets the rows per `INSERT`.
    #[must_use]
    pub fn with_rows_per_insert(mut self, rows: usize) -> Self {
        self.rows_per_insert = rows;
        self
    }

    /// Attaches the user's confirmation.
    #[must_use]
    pub fn confirmed(mut self, token: ImportToken) -> Self {
        self.confirmation = Some(token);
        self
    }
}

/// What the user confirms before an import on a production (or confirm-every-write)
/// connection.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ImportConfirmation {
    /// Token to send back with the same request.
    pub token: ImportToken,
    /// Connection name.
    pub connection_name: String,
    /// The connection is typed production.
    pub production: bool,
    /// The target table, quoted as it will be sent.
    pub table: String,
    /// The mapped target columns.
    pub columns: Vec<String>,
    /// The statement shape (one row; the import sends the same with more rows).
    pub statement: String,
    /// The file (resolved).
    pub file: PathBuf,
    /// Its size in bytes.
    pub file_bytes: u64,
}

/// How an import ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ImportEnd {
    /// Every row was imported (and committed).
    Completed,
    /// Stopped by the user.
    Cancelled,
    /// Stopped by an error ([`ImportSummary::error`]).
    Failed,
}

/// Summary of an import.
#[derive(Debug)]
#[non_exhaustive]
pub struct ImportSummary {
    /// How it ended.
    pub end: ImportEnd,
    /// The error that stopped it.
    pub error: Option<TransferJobError>,
    /// Data rows read from the file (valid or not).
    pub rows_read: u64,
    /// Rows the database accepted.
    pub rows_inserted: u64,
    /// Rows committed and kept (with all-or-nothing: all of them, or none).
    pub rows_committed: u64,
    /// Invalid rows skipped.
    pub rows_skipped: u64,
    /// The first issues ([`MAX_REPORTED_ISSUES`]): line, column, reason, never a value.
    pub issues: Vec<RowIssue>,
    /// All issues found.
    pub issues_total: u64,
    /// `INSERT` statements sent.
    pub statements: u64,
    /// Commits.
    pub commits: u64,
    /// The import ran in transactions.
    pub transactional: bool,
    /// Bytes of the file read.
    pub bytes_read: u64,
    /// Wall-clock time.
    pub elapsed: Duration,
}

/// Event of an import. Exactly one of [`ImportEvent::NeedsConfirmation`],
/// [`ImportEvent::Done`] or [`ImportEvent::Failed`] ends it.
#[derive(Debug)]
#[non_exhaustive]
pub enum ImportEvent {
    /// Nothing was sent: the import needs the user's confirmation (terminal).
    NeedsConfirmation(ImportConfirmation),
    /// Refused before anything was sent (terminal).
    Failed(TransferJobError),
    /// The import started.
    Started {
        /// File size in bytes.
        file_bytes: u64,
    },
    /// Progress (dropped when the UI reads slowly).
    Progress {
        /// Rows accepted so far.
        rows_inserted: u64,
        /// File bytes read so far.
        bytes_read: u64,
        /// File size.
        file_bytes: u64,
    },
    /// The import ended, completed or not (terminal).
    Done(ImportSummary),
}

/// A CSV preview request.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct CsvPreviewRequest {
    /// The file.
    pub path: PathBuf,
    /// Columns of the target table.
    pub targets: Vec<TargetColumn>,
    /// CSV syntax.
    pub format: CsvFormat,
    /// Caps.
    pub limits: CsvLimits,
    /// Value options.
    pub values: ValueOptions,
    /// How the mapping is suggested.
    pub mode: MappingMode,
    /// Rows to preview (at most 1000, default 100).
    pub rows: usize,
}

impl CsvPreviewRequest {
    /// A preview of the first 100 rows, mapped by name.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>, targets: Vec<TargetColumn>) -> Self {
        Self {
            path: path.into(),
            targets,
            format: CsvFormat::default(),
            limits: CsvLimits::default(),
            values: ValueOptions::default(),
            mode: MappingMode::ByName,
            rows: 100,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Service

#[derive(PartialEq)]
struct ImportKey {
    session: crate::session::SessionId,
    file: PathBuf,
    /// Identity of the opened file: a different file, or the same one changed, does not match.
    file_id: FileId,
    table: Vec<String>,
    columns: Vec<String>,
    /// Source field → target column pairs, in insert order.
    mapping: Vec<(usize, usize)>,
    transaction: ImportTransaction,
    skip_invalid_rows: bool,
}

/// `(device, inode, length, modification time)` of an opened import file (`fstat`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileId {
    dev: u64,
    ino: u64,
    len: u64,
    mtime: i64,
    mtime_nsec: i64,
}

/// An import file opened once (`O_NOFOLLOW | O_NONBLOCK`), checked with `fstat`; the reader
/// uses this handle, never the path again.
struct ImportFile {
    file: fs::File,
    path: PathBuf,
    id: FileId,
}

struct PendingImport {
    id: Uuid,
    key: ImportKey,
    issued: Instant,
}

/// Exports and imports data on sessions (see the [module documentation](self)). Cheap to clone.
#[derive(Clone)]
pub struct TransferService {
    manager: SessionManager,
    roots: DbeaverRoots,
    /// NineSQL's own directories and `~/.ssh`, resolved at every check.
    protected: Arc<Vec<(PathBuf, &'static str)>>,
    pending: Arc<Mutex<Vec<PendingImport>>>,
}

impl fmt::Debug for TransferService {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TransferService").finish_non_exhaustive()
    }
}

impl TransferService {
    /// A service over `manager`'s sessions, refusing NineSQL's own directories of `paths` and
    /// the manager's DBeaver roots.
    #[must_use]
    pub fn new(manager: SessionManager, paths: &Paths) -> Self {
        let mut protected: Vec<(PathBuf, &'static str)> =
            [paths.config_dir(), paths.data_dir(), paths.state_dir()]
                .iter()
                .map(|p| (p.to_path_buf(), NINESQL_STORAGE))
                .collect();
        if let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) {
            protected.push((PathBuf::from(home).join(".ssh"), SSH_DIR));
        }
        Self {
            roots: manager.dbeaver_roots().clone(),
            manager,
            protected: Arc::new(protected),
            pending: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Why `dir` (a resolved directory) or one of its ancestors is refused: it is, or lies
    /// inside, NineSQL's own storage or `~/.ssh`. Compared by `(device, inode)` of `dir` and of
    /// each ancestor against the protected directories as they are now, and by resolved path
    /// prefix (bind mounts and later renames are both covered).
    fn protected_reason(&self, dir: &Path) -> Option<&'static str> {
        use std::os::unix::fs::MetadataExt;
        let ids: Vec<((u64, u64), &'static str, PathBuf)> = self
            .protected
            .iter()
            .filter_map(|(p, reason)| {
                let canonical = fs::canonicalize(p).ok()?;
                let m = fs::metadata(&canonical).ok()?;
                Some(((m.dev(), m.ino()), *reason, canonical))
            })
            .collect();
        for (_, reason, canonical) in &ids {
            if dir.starts_with(canonical) {
                return Some(reason);
            }
        }
        for ancestor in dir.ancestors() {
            if let Ok(m) = fs::metadata(ancestor)
                && let Some((_, reason, _)) =
                    ids.iter().find(|(id, _, _)| *id == (m.dev(), m.ino()))
            {
                return Some(reason);
            }
        }
        None
    }

    /// Checks an export target and returns it with its directory resolved: absolute, an existing
    /// directory, not inside a DBeaver workspace (I4, fail-closed), not inside NineSQL's own
    /// storage (I1), not a symbolic link or a non-regular file, not an existing file unless
    /// `overwrite` is confirmed. Blocking (`stat`, `canonicalize`): call it off the UI thread.
    ///
    /// # Errors
    /// [`TransferError::InvalidPath`], [`TransferError::ForbiddenLocation`],
    /// [`TransferError::NotARegularFile`], [`TransferError::AlreadyExists`].
    pub fn check_export_path(
        &self,
        path: &Path,
        overwrite: Overwrite,
    ) -> Result<PathBuf, TransferError> {
        if !path.is_absolute() {
            return Err(TransferError::InvalidPath(
                "the path is not absolute".into(),
            ));
        }
        let name = path
            .file_name()
            .ok_or_else(|| TransferError::InvalidPath("the path has no file name".into()))?;
        let parent = path
            .parent()
            .ok_or_else(|| TransferError::InvalidPath("the path has no directory".into()))?;
        let parent = fs::canonicalize(parent)
            .map_err(|_| TransferError::InvalidPath("the directory does not exist".into()))?;
        let target = parent.join(name);
        if self.roots.check(&target).inside {
            return Err(TransferError::ForbiddenLocation {
                reason: "the folder is inside a DBeaver workspace, which NineSQL never modifies"
                    .into(),
            });
        }
        if let Some(reason) = self.protected_reason(&parent) {
            return Err(TransferError::ForbiddenLocation {
                reason: reason.into(),
            });
        }
        ninesql_transfer::file::check_target(&target, overwrite)?;
        Ok(target)
    }

    fn open_import_file(
        &self,
        path: &Path,
        limits: &CsvLimits,
    ) -> Result<ImportFile, TransferError> {
        use std::os::unix::fs::MetadataExt;
        let canonical = fs::canonicalize(path)
            .map_err(|_| TransferError::InvalidPath("the file does not exist".into()))?;
        if let Some(reason) = canonical.parent().and_then(|d| self.protected_reason(d)) {
            return Err(TransferError::ForbiddenLocation {
                reason: reason.into(),
            });
        }
        // Not opened at all unless it is a regular file (a FIFO or a device has side effects).
        if !fs::metadata(&canonical)?.is_file() {
            return Err(TransferError::NotARegularFile(canonical));
        }
        let (file, meta) = ninesql_transfer::file::open_input(&canonical)?;
        if meta.len() > limits.max_file_bytes {
            return Err(TransferError::FileTooLarge {
                limit: limits.max_file_bytes,
            });
        }
        Ok(ImportFile {
            file,
            path: canonical,
            id: FileId {
                dev: meta.dev(),
                ino: meta.ino(),
                len: meta.len(),
                mtime: meta.mtime(),
                mtime_nsec: meta.mtime_nsec(),
            },
        })
    }

    /// Exports rows to a file (see the [module documentation](self#export)).
    pub fn export(&self, session: &SessionHandle, request: ExportRequest) -> ExportJob {
        let (events, rx) = mpsc::channel(EVENT_QUEUE);
        let cancel = CancelToken::new();
        let task = ExportTask {
            service: self.clone(),
            session: session.clone(),
            request,
            events,
            cancel: cancel.clone(),
        };
        self.manager.runtime().spawn(task.run());
        Job { events: rx, cancel }
    }

    /// Reads the first rows of a CSV file and checks them against the target columns, on the
    /// blocking pool.
    pub fn preview_csv(
        &self,
        request: CsvPreviewRequest,
    ) -> Reply<Result<CsvPreview, TransferError>> {
        let (tx, reply) = Reply::channel();
        let service = self.clone();
        self.manager.runtime().handle().spawn_blocking(move || {
            let result = service
                .open_import_file(&request.path, &request.limits)
                .and_then(|opened| {
                    ninesql_transfer::import::preview(
                        BufReader::new(opened.file),
                        request.format,
                        request.limits,
                        &request.values,
                        &request.targets,
                        request.mode,
                        request.rows,
                    )
                });
            let _ = tx.send(result);
        });
        reply
    }

    /// Imports a CSV file into a table (see the [module documentation](self#import)).
    pub fn import_csv(&self, session: &SessionHandle, request: ImportRequest) -> ImportJob {
        let (events, rx) = mpsc::channel(EVENT_QUEUE);
        let cancel = CancelToken::new();
        let task = ImportTask {
            service: self.clone(),
            session: session.clone(),
            request,
            events,
            cancel: cancel.clone(),
        };
        self.manager.runtime().spawn(task.run());
        Job { events: rx, cancel }
    }

    fn issue_confirmation(&self, key: ImportKey) -> ImportToken {
        let mut pending = lock(&self.pending);
        pending.retain(|p| p.issued.elapsed() < CONFIRMATION_TTL);
        if pending.len() >= MAX_PENDING_CONFIRMATIONS {
            pending.remove(0);
        }
        let id = Uuid::new_v4();
        pending.push(PendingImport {
            id,
            key,
            issued: Instant::now(),
        });
        ImportToken(id)
    }

    fn redeem_confirmation(&self, token: ImportToken, key: &ImportKey) -> bool {
        let mut pending = lock(&self.pending);
        pending.retain(|p| p.issued.elapsed() < CONFIRMATION_TTL);
        match pending.iter().position(|p| p.id == token.0) {
            Some(i) => {
                // Single use, whether it matches or not.
                let p = pending.remove(i);
                p.key == *key
            }
            None => false,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Export task

/// Bytes of result batches held by an export, with the peak.
#[derive(Default)]
struct Gauge {
    current: AtomicU64,
    peak: AtomicU64,
}

impl Gauge {
    fn add(&self, n: u64) {
        let now = self.current.fetch_add(n, Ordering::SeqCst) + n;
        self.peak.fetch_max(now, Ordering::SeqCst);
    }

    fn sub(&self, n: u64) {
        self.current.fetch_sub(n, Ordering::SeqCst);
    }
}

fn item_size(item: &ResultItem) -> u64 {
    match item {
        ResultItem::Rows(b) => b.get_array_memory_size() as u64,
        _ => 64,
    }
}

enum WriterMsg {
    Item(ResultItem),
    End,
}

struct ExportTask {
    service: TransferService,
    session: SessionHandle,
    request: ExportRequest,
    events: mpsc::Sender<ExportEvent>,
    cancel: CancelToken,
}

impl ExportTask {
    async fn run(self) {
        let event = match self.run_inner().await {
            Ok(summary) => {
                tracing::info!(
                    rows = summary.rows,
                    bytes = summary.bytes,
                    "export finished"
                );
                ExportEvent::Done(summary)
            }
            Err(e) => {
                tracing::info!(kind = e.kind(), "export failed");
                ExportEvent::Failed(e)
            }
        };
        let _ = self.events.send(event).await;
    }

    async fn run_inner(&self) -> Result<ExportSummary, TransferJobError> {
        let started = Instant::now();
        let request = &self.request;
        let dialect = self.session.dialect();
        let (text, params) = match &request.source {
            ExportSource::Statement { text, params } => (text.clone(), params.clone()),
            ExportSource::Table { path } => {
                if path.is_empty() || path.iter().any(String::is_empty) {
                    return Err(TransferError::InvalidOption("no table to export".into()).into());
                }
                (
                    format!("SELECT * FROM {}", dialect.qualified_name(path)),
                    Vec::new(),
                )
            }
        };
        let class = dialect.classify(&text);
        if class != StatementClass::Read {
            return Err(TransferJobError::NotARead(class));
        }
        let mut options = request.options.clone();
        if options.format == ExportFormat::SqlInsert
            && options.sql.table.is_empty()
            && let ExportSource::Table { path } = &request.source
        {
            options.sql.table.clone_from(path);
        }
        options.validate()?;
        let renderer: Option<Arc<dyn SqlLiteralRenderer>> =
            (options.format == ExportFormat::SqlInsert).then(|| {
                Arc::new(DialectSql::new(Arc::clone(&dialect))) as Arc<dyn SqlLiteralRenderer>
            });

        // The session is the export's alone until it ends (review L9).
        let mut guard = ExportLease {
            lease: acquire_lease(&self.session)
                .await
                .ok_or(TransferJobError::SessionBusy)?,
            read_only_tx: false,
        };

        // The file: checked and created on the blocking pool.
        let service = self.service.clone();
        let path = request.path.clone();
        let overwrite = request.overwrite;
        let target = spawn_blocking(move || service.check_export_path(&path, overwrite)).await??;

        let gauge = Arc::new(Gauge::default());
        let written = Arc::new(AtomicU64::new(0));
        let (wtx, mut wrx) = mpsc::channel::<WriterMsg>(WRITER_QUEUE);
        let writer = {
            let gauge = Arc::clone(&gauge);
            let written = Arc::clone(&written);
            let cancel = self.cancel.clone();
            let target = target.clone();
            self.service.manager.runtime().handle().spawn_blocking(
                move || -> Result<ninesql_transfer::ExportStats, TransferError> {
                    let file = OutputFile::create(&target, overwrite)?;
                    let mut exporter = Exporter::new(file, options, renderer)?;
                    loop {
                        match wrx.blocking_recv() {
                            Some(WriterMsg::Item(item)) => {
                                let size = item_size(&item);
                                let result = exporter.write_item(&item);
                                drop(item);
                                gauge.sub(size);
                                result?;
                                written.store(exporter.bytes(), Ordering::Relaxed);
                                if cancel.is_cancelled() {
                                    // Dropping the exporter drops the file: the temp is removed.
                                    return Err(TransferError::Cancelled);
                                }
                            }
                            Some(WriterMsg::End) => {
                                let (file, stats) = exporter.finish()?;
                                file.commit()?;
                                return Ok(stats);
                            }
                            None => return Err(TransferError::Cancelled),
                        }
                    }
                },
            )
        };

        guard.read_only_tx = self.begin_read_only(&guard.lease, &dialect).await?;
        let budget = request.budget.with_max_bytes(u64::MAX).with_max_rows(None);
        let mut exec = guard.lease.execute(
            ExecuteRequest::sql(text)
                .with_params(params)
                .with_budget(budget)
                .with_timeout(request.timeout),
        );
        let _ = self.events.try_send(ExportEvent::Started);
        let mut rows = 0u64;
        let mut last_progress = Instant::now();
        let mut failure: Option<TransferJobError> = None;
        let mut ended = false;
        loop {
            let event = tokio::select! {
                biased;
                () = self.cancel.cancelled() => {
                    failure = Some(TransferJobError::Cancelled);
                    break;
                }
                event = exec.next() => event,
            };
            match event {
                Some(ExecEvent::Item(item)) => {
                    if let ResultItem::Rows(b) = &item {
                        rows += b.num_rows() as u64;
                    }
                    gauge.add(item_size(&item));
                    if wtx.send(WriterMsg::Item(item)).await.is_err() {
                        // The writer failed: its error is reported below.
                        break;
                    }
                    if last_progress.elapsed() >= PROGRESS_INTERVAL {
                        last_progress = Instant::now();
                        let _ = self.events.try_send(ExportEvent::Progress {
                            rows,
                            bytes: written.load(Ordering::Relaxed),
                        });
                    }
                }
                Some(ExecEvent::Finished { .. }) => {
                    ended = true;
                    let _ = wtx.send(WriterMsg::End).await;
                    break;
                }
                Some(ExecEvent::Failed(e)) => {
                    failure = Some(TransferJobError::from_core(e));
                    break;
                }
                Some(ExecEvent::NeedsConfirmation(_)) => {
                    failure = Some(TransferJobError::NotARead(class));
                    break;
                }
                Some(_) => {}
                None => {
                    failure = Some(TransferJobError::Core(CoreError::SessionClosed));
                    break;
                }
            }
        }
        // Stops the statement if it still runs; closing the queue without `End` aborts the file.
        drop(exec);
        drop(wtx);
        let written_stats = writer
            .await
            .map_err(|e| TransferJobError::Core(CoreError::Runtime(e.to_string())))?;
        if let Some(f) = failure {
            return Err(f);
        }
        if !ended {
            // The writer stopped first: its error says why.
            return Err(written_stats.err().map_or(
                TransferJobError::Core(CoreError::SessionClosed),
                TransferJobError::from,
            ));
        }
        let stats = written_stats?;
        Ok(ExportSummary {
            path: target,
            rows: stats.rows,
            bytes: stats.bytes,
            elapsed: started.elapsed(),
            truncated_value_columns: stats.truncated_value_columns,
            result_sets_skipped: stats.result_sets_skipped,
            incomplete: stats.incomplete.is_some(),
            peak_buffered_bytes: gauge.peak.load(Ordering::SeqCst),
        })
    }
}

/// The export's lease on its session; dropping it ends the read-only transaction the export
/// opened (the `ROLLBACK` is queued before the lease is released, so it runs first).
struct ExportLease {
    lease: SessionLease,
    read_only_tx: bool,
}

impl Drop for ExportLease {
    fn drop(&mut self) {
        if self.read_only_tx {
            drop(self.lease.rollback());
        }
    }
}

impl ExportTask {
    /// Opens an engine-side read-only transaction around the export when the session has none
    /// open and is not read-only already (review L7): PostgreSQL family `BEGIN READ ONLY`,
    /// MySQL family `START TRANSACTION READ ONLY`; SQLite and engines without transactions need
    /// or allow none. `true` when one was opened. If it cannot be opened, the export fails on a
    /// production or confirm-every-write connection and goes on without it otherwise.
    async fn begin_read_only(
        &self,
        lease: &SessionLease,
        dialect: &Arc<dyn ninesql_driver_api::Dialect>,
    ) -> Result<bool, TransferJobError> {
        let info = self.session.info();
        if info.read_only
            || !info.capabilities.transactions.manual_commit
            || self.session.transaction_state().in_transaction
        {
            return Ok(false);
        }
        let rules = dialect.lexical_rules();
        let text = if rules.e_string_backslash {
            "BEGIN READ ONLY"
        } else if rules.hash_comments {
            "START TRANSACTION READ ONLY"
        } else {
            return Ok(false);
        };
        let mut exec = lease.execute(ExecuteRequest::sql(text));
        let outcome = loop {
            match exec.next().await {
                Some(ExecEvent::Finished { .. }) => break Ok(()),
                Some(ExecEvent::Failed(e)) => break Err(TransferJobError::from_core(e)),
                Some(ExecEvent::NeedsConfirmation(_)) => {
                    break Err(TransferJobError::UnexpectedConfirmation);
                }
                Some(_) => {}
                None => break Err(TransferJobError::Core(CoreError::SessionClosed)),
            }
        };
        match outcome {
            Ok(()) => Ok(true),
            Err(e) if info.policy.production || info.policy.confirm_every_write => Err(e),
            Err(e) => {
                tracing::warn!(
                    kind = e.kind(),
                    "export runs without a read-only transaction"
                );
                Ok(false)
            }
        }
    }
}

/// How long a transfer waits for its session to finish what it is doing (the end of the
/// previous statement is reported a moment before the session task is idle again).
const LEASE_WAIT: Duration = Duration::from_secs(2);

/// Leases `session`, waiting up to [`LEASE_WAIT`] for it to become idle; `None` when another
/// transfer holds it or it stays busy (a running or paused statement).
async fn acquire_lease(session: &SessionHandle) -> Option<SessionLease> {
    let deadline = Instant::now() + LEASE_WAIT;
    loop {
        if let Some(lease) = session.lease() {
            return Some(lease);
        }
        if session.is_leased() || session.is_closed() || Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn spawn_blocking<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, TransferJobError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| TransferJobError::Core(CoreError::Runtime(e.to_string())))
}

// ---------------------------------------------------------------------------------------------
// Import task

struct ImportTask {
    service: TransferService,
    session: SessionHandle,
    request: ImportRequest,
    events: mpsc::Sender<ImportEvent>,
    cancel: CancelToken,
}

/// Counters of a running import.
#[derive(Default)]
struct ImportCounters {
    rows_read: u64,
    rows_inserted: u64,
    rows_committed: u64,
    rows_skipped: u64,
    issues: Vec<RowIssue>,
    issues_total: u64,
    statements: u64,
    commits: u64,
    since_commit: u64,
    bytes_read: u64,
}

impl ImportCounters {
    fn record(&mut self, issue: RowIssue) {
        self.issues_total += 1;
        if self.issues.len() < MAX_REPORTED_ISSUES {
            self.issues.push(issue);
        }
    }
}

enum ReaderMsg {
    Batch(RowBatch, u64),
    Error(TransferError),
}

impl ImportTask {
    async fn run(self) {
        let event = self.run_inner().await;
        if let ImportEvent::Done(s) = &event {
            tracing::info!(
                end = ?s.end,
                rows = s.rows_inserted,
                committed = s.rows_committed,
                skipped = s.rows_skipped,
                "import ended"
            );
        }
        let _ = self.events.send(event).await;
    }

    fn rows_per_insert(&self) -> usize {
        let columns = self.request.mapping.columns.len().max(1);
        self.request
            .rows_per_insert
            .clamp(1, MAX_ROWS_PER_INSERT)
            .min((MAX_PARAMS_PER_INSERT / columns).max(1))
    }

    #[allow(clippy::too_many_lines)] // One linear flow: checks, confirmation, loop, end.
    async fn run_inner(&self) -> ImportEvent {
        let started = Instant::now();
        let request = &self.request;
        let info = self.session.info().clone();
        // Refusals before anything is read or sent.
        if info.read_only {
            return ImportEvent::Failed(TransferJobError::ReadOnlySession);
        }
        if !info.capabilities.parameters {
            return ImportEvent::Failed(TransferJobError::NoParameters);
        }
        let transactional = info.capabilities.transactions.manual_commit;
        if request.transaction == ImportTransaction::AllOrNothing && !transactional {
            return ImportEvent::Failed(TransferJobError::NoTransactions);
        }
        if let ImportTransaction::CommitEvery(0) = request.transaction {
            return ImportEvent::Failed(
                TransferError::InvalidOption("commit every 0 rows".into()).into(),
            );
        }
        if self.session.transaction_state().in_transaction {
            return ImportEvent::Failed(TransferJobError::PendingTransaction);
        }
        if let Err(e) = request.mapping.validate(&request.targets) {
            return ImportEvent::Failed(e.into());
        }
        if request.table.is_empty() || request.table.iter().any(String::is_empty) {
            return ImportEvent::Failed(
                TransferError::InvalidOption("no target table".into()).into(),
            );
        }
        let service = self.service.clone();
        let path = request.path.clone();
        let limits = request.limits;
        let opened = match spawn_blocking(move || service.open_import_file(&path, &limits)).await {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => return ImportEvent::Failed(e.into()),
            Err(e) => return ImportEvent::Failed(e),
        };
        let (file, file_bytes, file_id) = (opened.path.clone(), opened.id.len, opened.id);
        let columns: Vec<String> = request
            .mapping
            .columns
            .iter()
            .map(|c| request.targets[c.target].name.clone())
            .collect();
        let renderer = DialectSql::new(self.session.dialect());
        let template = match renderer.insert(&request.table, &columns, 1) {
            Ok(t) => t,
            Err(e) => return ImportEvent::Failed(e.into()),
        };

        // Confirmation (ADR-0007): an import is a write.
        let confirmed = if info.policy.production || info.policy.confirm_every_write {
            let key = ImportKey {
                session: info.id,
                file: file.clone(),
                file_id,
                table: request.table.clone(),
                columns: columns.clone(),
                mapping: request
                    .mapping
                    .columns
                    .iter()
                    .map(|c| (c.source, c.target))
                    .collect(),
                transaction: request.transaction,
                skip_invalid_rows: request.skip_invalid_rows,
            };
            match request.confirmation {
                None => {
                    let token = self.service.issue_confirmation(key);
                    return ImportEvent::NeedsConfirmation(ImportConfirmation {
                        token,
                        connection_name: info.connection_name.clone(),
                        production: info.policy.production,
                        table: self.session.dialect().qualified_name(&request.table),
                        columns,
                        statement: template,
                        file,
                        file_bytes,
                    });
                }
                Some(token) => {
                    if !self.service.redeem_confirmation(token, &key) {
                        return ImportEvent::Failed(TransferJobError::InvalidConfirmation);
                    }
                    tracing::info!(session = %info.id, "import confirmed");
                    true
                }
            }
        } else {
            false
        };

        // The session is the import's alone until it ends (review L9).
        let Some(lease) = acquire_lease(&self.session).await else {
            return ImportEvent::Failed(TransferJobError::SessionBusy);
        };
        if self.session.transaction_state().in_transaction {
            return ImportEvent::Failed(TransferJobError::PendingTransaction);
        }

        // The reader: one statement's worth of rows at a time, on the blocking pool, from the
        // handle opened above (never the path again).
        let rows_per_insert = self.rows_per_insert();
        let (rtx, mut rrx) = mpsc::channel::<ReaderMsg>(1);
        {
            let (format, limits, values) = (request.format, request.limits, request.values.clone());
            let (targets, mapping) = (request.targets.clone(), request.mapping.clone());
            let handle = opened.file;
            let stop_first = !request.skip_invalid_rows;
            let cancel = self.cancel.clone();
            self.service
                .manager
                .runtime()
                .handle()
                .spawn_blocking(move || {
                    let opened = RowSource::open(
                        BufReader::new(handle),
                        format,
                        limits,
                        values,
                        targets,
                        mapping,
                    )
                    .map(|s| s.stopping_at_first_issue(stop_first));
                    let mut source = match opened {
                        Ok(s) => s,
                        Err(e) => {
                            let _ = rtx.blocking_send(ReaderMsg::Error(e));
                            return;
                        }
                    };
                    loop {
                        if cancel.is_cancelled() {
                            return;
                        }
                        match source.next_batch(rows_per_insert, MAX_INSERT_BYTES) {
                            Ok(Some(batch)) => {
                                let read = source.bytes_read();
                                if rtx.blocking_send(ReaderMsg::Batch(batch, read)).is_err() {
                                    return;
                                }
                            }
                            Ok(None) => return,
                            Err(e) => {
                                let _ = rtx.blocking_send(ReaderMsg::Error(e));
                                return;
                            }
                        }
                    }
                });
        }

        let _ = self.events.try_send(ImportEvent::Started { file_bytes });
        let mut c = ImportCounters::default();
        let mut in_tx = false;
        let mut last_progress = Instant::now();
        let outcome: Result<(), TransferJobError> = async {
            loop {
                let msg = tokio::select! {
                    biased;
                    () = self.cancel.cancelled() => return Err(TransferJobError::Cancelled),
                    msg = rrx.recv() => msg,
                };
                let (batch, read) = match msg {
                    None => break,
                    Some(ReaderMsg::Error(e)) => return Err(e.into()),
                    Some(ReaderMsg::Batch(b, read)) => (b, read),
                };
                c.bytes_read = read;
                c.rows_read += (batch.rows.len() + batch.issues.len()) as u64;
                if !batch.issues.is_empty() {
                    let first = batch.issues[0].describe();
                    let stop = !request.skip_invalid_rows;
                    c.rows_skipped += batch.issues.len() as u64;
                    for issue in batch.issues {
                        c.record(issue);
                    }
                    if stop {
                        return Err(TransferJobError::InvalidRow(first));
                    }
                }
                if batch.rows.is_empty() {
                    continue;
                }
                if transactional && !in_tx {
                    tx_op(lease.begin()).await?;
                    in_tx = true;
                }
                let n = batch.rows.len();
                let text = renderer.insert(&request.table, &columns, n)?;
                let first_line = batch.lines.first().copied().unwrap_or(0);
                let params: Vec<Param> = batch.rows.into_iter().flatten().collect();
                if let Err(e) = self
                    .insert(&lease, text, params, confirmed, request.statement_timeout)
                    .await
                {
                    if !matches!(e, TransferJobError::Cancelled) {
                        c.record(RowIssue {
                            line: first_line,
                            kind: IssueKind::Rejected,
                        });
                    }
                    return Err(e);
                }
                c.statements += 1;
                c.rows_inserted += n as u64;
                c.since_commit += n as u64;
                if !transactional {
                    c.rows_committed += n as u64;
                } else if let ImportTransaction::CommitEvery(every) = request.transaction
                    && c.since_commit >= every
                {
                    tx_op(lease.commit()).await?;
                    in_tx = false;
                    c.commits += 1;
                    c.rows_committed = c.rows_inserted;
                    c.since_commit = 0;
                }
                if last_progress.elapsed() >= PROGRESS_INTERVAL {
                    last_progress = Instant::now();
                    let _ = self.events.try_send(ImportEvent::Progress {
                        rows_inserted: c.rows_inserted,
                        bytes_read: c.bytes_read,
                        file_bytes,
                    });
                }
            }
            if in_tx {
                tx_op(lease.commit()).await?;
                in_tx = false;
                c.commits += 1;
                c.rows_committed = c.rows_inserted;
            }
            Ok(())
        }
        .await;
        // Stop the reader.
        rrx.close();
        let (end, error) = match outcome {
            Ok(()) => (ImportEnd::Completed, None),
            Err(e) => {
                if in_tx {
                    // A failed rollback leaves the transaction to the session (closing it rolls
                    // back); it is reported through the session's transaction state.
                    if let Err(re) = tx_op(lease.rollback()).await {
                        tracing::warn!(kind = re.kind(), "rollback after a failed import failed");
                    }
                }
                let end = if matches!(e, TransferJobError::Cancelled) {
                    ImportEnd::Cancelled
                } else {
                    ImportEnd::Failed
                };
                (end, Some(e))
            }
        };
        ImportEvent::Done(ImportSummary {
            end,
            error,
            rows_read: c.rows_read,
            rows_inserted: c.rows_inserted,
            rows_committed: c.rows_committed,
            rows_skipped: c.rows_skipped,
            issues: c.issues,
            issues_total: c.issues_total,
            statements: c.statements,
            commits: c.commits,
            transactional,
            bytes_read: c.bytes_read,
            elapsed: started.elapsed(),
        })
    }

    /// Sends one `INSERT`, answering the session's per-statement confirmation for this exact
    /// statement when the import was confirmed.
    async fn insert(
        &self,
        lease: &SessionLease,
        text: String,
        params: Vec<Param>,
        confirmed: bool,
        timeout: Option<Duration>,
    ) -> Result<(), TransferJobError> {
        let request = ExecuteRequest::sql(text)
            .with_params(params)
            .with_timeout(timeout);
        let mut attempt = 0;
        let mut current = request.clone();
        loop {
            attempt += 1;
            match self.run_statement(lease, current).await? {
                None => return Ok(()),
                Some(confirmation) => {
                    let expected = confirmation.statement == request.text
                        && confirmation.params == request.params.len()
                        && confirmation.class == StatementClass::Write;
                    if !confirmed || !expected || attempt > 1 {
                        return Err(TransferJobError::UnexpectedConfirmation);
                    }
                    current = request.clone().confirmed(confirmation.token);
                }
            }
        }
    }

    /// Runs one statement to its end: `Ok(None)` when it finished, `Ok(Some(_))` when the session
    /// asks for a confirmation.
    async fn run_statement(
        &self,
        lease: &SessionLease,
        request: ExecuteRequest,
    ) -> Result<Option<crate::session::ConfirmationRequest>, TransferJobError> {
        let mut exec = lease.execute(request);
        loop {
            let event = tokio::select! {
                biased;
                () = self.cancel.cancelled() => {
                    exec.cancel();
                    return Err(TransferJobError::Cancelled);
                }
                event = exec.next() => event,
            };
            match event {
                Some(ExecEvent::Finished { .. }) => return Ok(None),
                Some(ExecEvent::Failed(e)) => return Err(TransferJobError::from_core(e)),
                Some(ExecEvent::NeedsConfirmation(c)) => return Ok(Some(c)),
                Some(_) => {}
                None => return Err(TransferJobError::Core(CoreError::SessionClosed)),
            }
        }
    }
}

async fn tx_op(
    reply: Reply<Result<crate::session::TransactionState, CoreError>>,
) -> Result<(), TransferJobError> {
    match reply.await {
        Some(Ok(_)) => Ok(()),
        Some(Err(e)) => Err(TransferJobError::Core(e)),
        None => Err(TransferJobError::Core(CoreError::SessionClosed)),
    }
}
