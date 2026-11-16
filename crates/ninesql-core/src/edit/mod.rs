//! Row editing, filtered table queries and foreign-key navigation, core side (P5-A:
//! F-DATA-04, F-DATA-03, F-DATA-07).
//!
//! # Editing (F-DATA-04)
//! ```text
//! let table = core.edit().describe_table(&session, path).await?;   // key from the metadata
//! let mut edits = PendingEdits::new(table, &result_schema)?;        // one per result tab
//! edits.set_value(row, &batch, batch_row, field, edits.parse_input(field, "42")?)?;
//! edits.delete_row(row2, &batch, batch_row2)?;
//! let plan = edits.plan(&*session.dialect(), &session.info().capabilities.editing)?;
//! // show plan.script (the DML preview) and plan.unchecked_columns
//! let mut job = core.edit().save(&session, &edits, SaveOptions::default());
//! match job.next().await { Some(SaveEvent::NeedsConfirmation(c)) => /* show c.script, then */
//!     core.edit().save(&session, &edits, SaveOptions::default().confirmed(c.token)), … }
//! ```
//! - **Key** ([`EditService::describe_table`], [`EditableTable::from_metadata`]): from the
//!   metadata service, the table's unique indexes through `KeyCandidate::from_index` (the
//!   driver's flags only) and SQLite's `INTEGER PRIMARY KEY`; the primary key first, then the
//!   smallest non-null unique key, then the smallest nullable one. Without a key, or when the
//!   session is read-only, the driver declares no editing (I8), has no transactions or no bind
//!   parameters, or the table's storage engine is not transactional, editing is unavailable
//!   with a [`NotEditable`] explanation.
//! - **Pending changes** ([`PendingEdits`]): cell edits, inserted, duplicated and deleted rows;
//!   [`PendingEdits::plan`] renders them with the P5-B generator (`generate_batch`) into the
//!   statements and the DML preview ([`SavePlan::script`]).
//! - **Save** ([`EditService::save`]): never on a read-only session; the session is **leased**
//!   for the whole save (every other statement on it fails with `SessionLeased` meanwhile); each
//!   statement runs in its **bound** form with [`DmlStatement::check_affected`] (exactly one
//!   row); any error, any other row count or a cancel **undoes the whole save**:
//!   - auto-commit session: `BEGIN` … `COMMIT` of its own, rolled back on failure (an open
//!     transaction there is refused);
//!   - manual-commit session (production by default, ADR-0007 §5): inside the session's
//!     transaction (opened with `BEGIN` when none is), around `SAVEPOINT ninesql_edit_save`;
//!     on failure `ROLLBACK TO SAVEPOINT` (or a full rollback when the save opened the
//!     transaction), on success `RELEASE SAVEPOINT` and the transaction stays open for the user
//!     to commit ([`SaveSummary::committed`] is `false`). A driver without savepoints refuses a
//!     save while a transaction is open.
//!
//!   `UPDATE` / `DELETE` carry the original values of the edited columns
//!   (`Concurrency::EditedColumns`); columns without reliable equality, or whose values were
//!   cut, are reported in [`SavePlan::unchecked_columns`] / [`SaveSummary::unchecked_columns`].
//! - **Production (ADR-0007).** On a connection typed production, or whose type confirms every
//!   write, the first call ends with [`SaveEvent::NeedsConfirmation`]: **one confirmation for
//!   the whole save**, showing the preview script, with a single-use [`SaveToken`] bound to the
//!   session, the table and the exact statements and values (ten minutes), the design of the
//!   P5-C import confirmation. Once confirmed, the per-statement confirmation the session asks
//!   for each statement (types that confirm every write) is answered by the service for the
//!   confirmed statements only; anything else fails the save. A statement the dialect classifies
//!   as anything but a filtered write never runs (the P5-B self-check).
//! - **Non-transactional tables** (`TableInfo::transactional == Some(false)`): refused, with
//!   no override, since a rollback would undo nothing.
//!
//! # Sort and filter (F-DATA-03)
//! Client-side sort and filter of the loaded rows are the grid's. Server-side,
//! [`build_table_query`] (or [`EditService::table_query`] with the session's dialect) re-runs a
//! single-table `SELECT` with a `WHERE` and an `ORDER BY` built safely: names quoted by the
//! dialect, values bound, a user-typed raw `WHERE` accepted only as text that stays a read
//! inside its parentheses ([`query`]). Run the [`BuiltQuery`] through the execution service
//! with its parameters. [`EditService::record_filter`] keeps a per-connection filter history
//! under the history rules ([`history`]).
//!
//! # Foreign-key navigation (F-DATA-07)
//! [`referenced_rows_query`] and [`referencing_rows_query`] build the bound `SELECT` of the rows
//! a row's foreign key points to, and of the rows pointing to a row; the foreign keys come from
//! the metadata service (`ObjectKind::ForeignKey` under a table, `ForeignKeyInfo`; the keys
//! pointing to a table with [`EditService::referencing_foreign_keys`]), the row's values from
//! [`row_values_by_name`].
//!
//! Nothing here logs a value (I1): logs carry counts, kinds and session ids.

pub mod history;
pub mod key;
pub mod pending;
pub mod query;

use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use ninesql_driver_api::arrow_array::RecordBatch;
use ninesql_driver_api::dml::{DmlError, DmlStatement, cell_value};
use ninesql_driver_api::metadata::{ObjectDetails, ObjectPath};
use ninesql_driver_api::{CancelToken, DriverError, ObjectKind, Param, ResultItem, StatementClass};
use tokio::sync::mpsc;
use uuid::Uuid;

pub use self::history::{FilterHistoryEntry, FilterRecorded, MAX_FILTERS_PER_CONNECTION};
pub use self::key::{EditableTable, NotEditable, RowKey, TableColumn, table_parts};
pub use self::pending::{ColumnStatus, NewRowId, PendingEdits, RowId, SavePlan};
pub use self::query::{
    BuiltQuery, ColumnFilter, FilterOp, QueryError, SortDirection, SortKey, TableQuery,
    build_table_query, check_raw_where, referenced_rows_query, referencing_rows_query,
};
use crate::connection::ConnectionId;
use crate::error::CoreError;
use crate::execution::ExecutionService;
use crate::metadata::{MetadataError, MetadataService, NodeKey};
use crate::session::{
    ConfirmationRequest, ExecEvent, ExecuteRequest, Reply, SessionHandle, SessionId, SessionInfo,
    SessionLease, SessionManager,
};
use crate::transfer::Job;

/// Lifetime of a save confirmation.
const CONFIRMATION_TTL: Duration = Duration::from_secs(10 * 60);
/// Save confirmations kept.
const MAX_PENDING_CONFIRMATIONS: usize = 16;
/// Events buffered per save before progress events are dropped.
const EVENT_QUEUE: usize = 16;
/// Savepoint around a save inside the session's transaction (manual-commit mode).
const SAVEPOINT: &str = "ninesql_edit_save";
/// Time allowed to a `SAVEPOINT`, `RELEASE` or `ROLLBACK TO`.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(30);

/// How a save uses transactions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TxMode {
    /// Auto-commit session: `BEGIN` … `COMMIT` of its own.
    Own,
    /// Manual-commit session: inside the session's transaction (`opened`: the save opened it),
    /// around a savepoint; left open for the user to commit.
    Savepoint { opened: bool },
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Why an edit, a save or a table description failed. Never holds a value from the data.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum EditError {
    /// Rows cannot be edited here (I8): the explanation to show.
    #[error(transparent)]
    NotEditable(#[from] NotEditable),
    /// No such result column.
    #[error("no such column")]
    NoSuchColumn,
    /// The column is not a column of the edited table.
    #[error("the column {0} cannot be edited (it is not a column of the table)")]
    ColumnNotEditable(String),
    /// A value (the cell, or a key value of its row) was cut at the fetch budget.
    #[error("the value of {0} was cut when it was read: it cannot be edited or used as a key")]
    ValueCut(String),
    /// The row is marked for deletion.
    #[error("the row is marked for deletion")]
    RowDeleted,
    /// No such added row.
    #[error("no such new row")]
    NoSuchRow,
    /// The batch does not belong to the edited result.
    #[error("the rows do not belong to the edited result")]
    SchemaMismatch,
    /// The input is not a value of the column's type.
    #[error("not a valid {0} value")]
    InvalidInput(&'static str),
    /// Nothing to save.
    #[error("there are no changes to save")]
    NothingToSave,
    /// The P5-B generator refused a change.
    #[error(transparent)]
    Dml(#[from] DmlError),
    /// The metadata could not be read.
    #[error(transparent)]
    Metadata(#[from] MetadataError),
    /// A session or driver error.
    #[error(transparent)]
    Core(#[from] CoreError),
    /// A transaction is open on the session: the save owns its transaction.
    #[error("a transaction is open on this session: commit or roll back first")]
    PendingTransaction,
    /// The session runs or queues other work, or a transfer holds it.
    #[error("the session is busy: wait for its statement to end")]
    SessionBusy,
    /// The confirmation does not match this save, or expired.
    #[error("the confirmation does not match these changes, or expired; confirm them again")]
    InvalidConfirmation,
    /// The session asked to confirm a statement the save did not expect.
    #[error("the database session asked to confirm a statement the save did not expect")]
    UnexpectedConfirmation,
    /// A statement did not change exactly one row: everything was rolled back.
    #[error("change {index} of the save: {source}; nothing was saved")]
    RowCount {
        /// 1-based position of the statement in the save.
        index: usize,
        /// The count error.
        source: DmlError,
    },
    /// The save was cancelled; nothing was saved.
    #[error("cancelled; nothing was saved")]
    Cancelled,
}

impl EditError {
    /// A short, stable kind, safe to log.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::NotEditable(_) => "not-editable",
            Self::NoSuchColumn => "no-such-column",
            Self::ColumnNotEditable(_) => "column-not-editable",
            Self::ValueCut(_) => "value-cut",
            Self::RowDeleted => "row-deleted",
            Self::NoSuchRow => "no-such-row",
            Self::SchemaMismatch => "schema-mismatch",
            Self::InvalidInput(_) => "invalid-input",
            Self::NothingToSave => "nothing-to-save",
            Self::Dml(_) => "dml",
            Self::Metadata(e) => e.kind(),
            Self::Core(e) => e.kind(),
            Self::PendingTransaction => "pending-transaction",
            Self::SessionBusy => "session-busy",
            Self::InvalidConfirmation => "invalid-confirmation",
            Self::UnexpectedConfirmation => "unexpected-confirmation",
            Self::RowCount { .. } => "row-count",
            Self::Cancelled => "cancelled",
        }
    }

    fn from_core(e: CoreError) -> Self {
        match e {
            CoreError::Driver(DriverError::Cancelled) => Self::Cancelled,
            other => Self::Core(other),
        }
    }
}

/// Session-level reasons rows cannot be edited (I3, I8), before any table is looked at.
///
/// # Errors
/// [`NotEditable::ReadOnlySession`], [`NotEditable::NotSupported`],
/// [`NotEditable::NoTransactions`], [`NotEditable::NoParameters`].
pub fn session_editable(info: &SessionInfo) -> Result<(), NotEditable> {
    if info.read_only {
        return Err(NotEditable::ReadOnlySession);
    }
    if !info.capabilities.editing.any() {
        return Err(NotEditable::NotSupported);
    }
    if !info.capabilities.transactions.manual_commit {
        return Err(NotEditable::NoTransactions);
    }
    if !info.capabilities.parameters {
        return Err(NotEditable::NoParameters);
    }
    Ok(())
}

/// The values of the result columns `names` (in that order) in row `row` of `batch`, read with
/// `cell_value` (for foreign-key navigation: the referencing columns of a key, or the
/// referenced ones).
///
/// # Errors
/// [`EditError::NoSuchColumn`] when a name is not in the batch (or twice);
/// [`EditError::ValueCut`]; [`EditError::Dml`] for a type with no parameter mapping.
pub fn row_values_by_name(
    batch: &RecordBatch,
    row: usize,
    names: &[String],
) -> Result<Vec<Param>, EditError> {
    let schema = batch.schema();
    names
        .iter()
        .map(|n| {
            let mut found = schema
                .fields()
                .iter()
                .enumerate()
                .filter(|(_, f)| f.name() == n);
            let (i, _) = found.next().ok_or(EditError::NoSuchColumn)?;
            if found.next().is_some() {
                return Err(EditError::NoSuchColumn);
            }
            cell_value(batch, i, row).map_err(|e| match e {
                DmlError::TruncatedValue(name) => EditError::ValueCut(name),
                other => EditError::Dml(other),
            })
        })
        .collect()
}

/// Single-use permission to run one save ([`SaveEvent::NeedsConfirmation`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SaveToken(Uuid);

/// Options of [`EditService::save`].
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct SaveOptions {
    /// Wall-clock limit of each statement (`None`: none).
    pub statement_timeout: Option<Duration>,
    /// The user's confirmation, when the first call asked for one.
    pub confirmation: Option<SaveToken>,
}

impl SaveOptions {
    /// Attaches the user's confirmation.
    #[must_use]
    pub fn confirmed(mut self, token: SaveToken) -> Self {
        self.confirmation = Some(token);
        self
    }

    /// Sets the per-statement timeout.
    #[must_use]
    pub fn with_statement_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.statement_timeout = timeout;
        self
    }
}

/// What the user confirms before a save on a production (or confirm-every-write) connection.
/// `Debug` hides the script.
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SaveConfirmation {
    /// Token to send back with the same changes ([`SaveOptions::confirmed`]).
    pub token: SaveToken,
    /// Connection name.
    pub connection_name: String,
    /// The connection is typed production.
    pub production: bool,
    /// The table, quoted as the statements spell it.
    pub table: String,
    /// The DML preview of every statement ([`SavePlan::script`]).
    pub script: String,
    /// Statements.
    pub statements: usize,
    /// Rows deleted, updated, inserted.
    pub deletes: usize,
    /// Rows updated.
    pub updates: usize,
    /// Rows inserted.
    pub inserts: usize,
    /// Columns left out of the concurrency guard.
    pub unchecked_columns: Vec<String>,
}

impl fmt::Debug for SaveConfirmation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SaveConfirmation")
            .field("token", &self.token)
            .field("production", &self.production)
            .field("statements", &self.statements)
            .field("script_len", &self.script.len())
            .finish_non_exhaustive()
    }
}

/// Summary of a save that ended well.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct SaveSummary {
    /// The save committed its own transaction (auto-commit sessions). `false` in manual-commit
    /// mode: the changes are in the session's open transaction, to commit or roll back like
    /// any other statement (ADR-0007 §5).
    pub committed: bool,
    /// Statements run (and committed).
    pub statements: usize,
    /// Rows changed (one per statement).
    pub rows_affected: u64,
    /// Columns left out of the concurrency guard.
    pub unchecked_columns: Vec<String>,
    /// Wall-clock time.
    pub elapsed: Duration,
}

/// Event of a save. Exactly one of [`SaveEvent::NeedsConfirmation`], [`SaveEvent::Done`] or
/// [`SaveEvent::Failed`] ends it.
#[derive(Debug)]
#[non_exhaustive]
pub enum SaveEvent {
    /// Nothing was sent: the save needs the user's confirmation (terminal).
    NeedsConfirmation(SaveConfirmation),
    /// The transaction was opened.
    Started {
        /// Statements to run.
        statements: usize,
    },
    /// Progress (dropped when the UI reads slowly).
    Progress {
        /// Statements done.
        done: usize,
        /// Statements in all.
        total: usize,
    },
    /// Every statement ran and passed its check (terminal): committed, or left in the session's
    /// open transaction in manual-commit mode ([`SaveSummary::committed`]). Refresh the result
    /// to see the rows as saved.
    Done(SaveSummary),
    /// Nothing was saved: refused before anything was sent, or rolled back (terminal). If the
    /// rollback itself failed, the session's transaction state says so (closing the session
    /// rolls back).
    Failed(EditError),
}

/// A running save. Dropping it cancels the save (and rolls it back).
pub type SaveJob = Job<SaveEvent>;

#[derive(PartialEq)]
struct SaveKey {
    session: SessionId,
    table: Vec<String>,
    statements: Vec<(String, Vec<Param>)>,
}

struct PendingSave {
    id: Uuid,
    key: SaveKey,
    issued: Instant,
}

/// Row editing, filtered table queries, foreign-key navigation and the filter history (see the
/// [module documentation](self)). Cheap to clone; every method returns at once.
#[derive(Clone)]
pub struct EditService {
    manager: SessionManager,
    metadata: MetadataService,
    execution: ExecutionService,
    pending: Arc<Mutex<Vec<PendingSave>>>,
    filters: Arc<Mutex<history::FilterHistory>>,
}

impl fmt::Debug for EditService {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EditService").finish_non_exhaustive()
    }
}

impl EditService {
    /// A service over `manager`'s sessions, reading keys through `metadata` and the history
    /// preferences from `execution`.
    #[must_use]
    pub fn new(
        manager: SessionManager,
        metadata: MetadataService,
        execution: ExecutionService,
    ) -> Self {
        Self {
            manager,
            metadata,
            execution,
            pending: Arc::new(Mutex::new(Vec::new())),
            filters: Arc::new(Mutex::new(history::FilterHistory::default())),
        }
    }

    /// Whether rows of the table at `path` can be edited on `session`, and by which key: the
    /// session checks of [`session_editable`], then the table's listing entry, columns and
    /// indexes from the metadata service (bound to the connection's oldest open session),
    /// then [`EditableTable::from_metadata`]. Columns hidden by a navigator filter count as
    /// unread ([`NotEditable::IncompleteMetadata`]).
    pub fn describe_table(
        &self,
        session: &SessionHandle,
        path: ObjectPath,
    ) -> Reply<Result<EditableTable, EditError>> {
        let (tx, reply) = Reply::channel();
        let metadata = self.metadata.clone();
        let info = session.info().clone();
        self.manager.runtime().spawn(async move {
            let result = describe(&metadata, &info, path).await;
            let _ = tx.send(result);
        });
        reply
    }

    /// Saves `edits` on `session` (see the [module documentation](self#editing-f-data-04)). The
    /// statements are generated now, with the session's dialect.
    pub fn save(
        &self,
        session: &SessionHandle,
        edits: &PendingEdits,
        options: SaveOptions,
    ) -> SaveJob {
        let (events, rx) = mpsc::channel(EVENT_QUEUE);
        let cancel = CancelToken::new();
        let plan = edits.plan(&*session.dialect(), &session.info().capabilities.editing);
        let task = SaveTask {
            service: self.clone(),
            session: session.clone(),
            table: edits.table().table.clone(),
            plan,
            options,
            events,
            cancel: cancel.clone(),
        };
        self.manager.runtime().spawn(task.run());
        Job::new(rx, cancel)
    }

    /// The foreign keys that reference the table at `path` (for "rows referencing this row",
    /// F-DATA-07): the foreign keys of the tables of the same schema (or the same parent), read
    /// through the metadata service (cached, bounded), at most [`MAX_REFERENCING_SCAN`] tables.
    /// Keys from other schemas are not found; [`ReferencingKeys::complete`] is `false` when the
    /// scan stopped at the bound or the table list was incomplete.
    pub fn referencing_foreign_keys(
        &self,
        session: &SessionHandle,
        path: ObjectPath,
    ) -> Reply<Result<ReferencingKeys, EditError>> {
        let (tx, reply) = Reply::channel();
        let metadata = self.metadata.clone();
        let connection = session.info().connection;
        self.manager.runtime().spawn(async move {
            let result = referencing(&metadata, connection, path).await;
            let _ = tx.send(result);
        });
        reply
    }

    /// [`build_table_query`] with the session's dialect (its current lexical modes).
    ///
    /// # Errors
    /// [`QueryError`].
    pub fn table_query(
        &self,
        session: &SessionHandle,
        query: &TableQuery,
    ) -> Result<BuiltQuery, QueryError> {
        build_table_query(&*session.dialect(), query)
    }

    /// Records a filter the user applied on `session`'s connection in the filter history,
    /// under the history rules ([`history`]); answers what was kept.
    #[must_use]
    pub fn record_filter(&self, session: &SessionHandle, query: &TableQuery) -> FilterRecorded {
        let info = session.info();
        let prefs = self.execution.preferences().history;
        lock(&self.filters).record(info.connection, &info.policy, &prefs, query)
    }

    /// The filter history of a connection, newest first, for one table or all.
    #[must_use]
    pub fn filter_history(
        &self,
        connection: ConnectionId,
        table: Option<&[String]>,
    ) -> Vec<FilterHistoryEntry> {
        lock(&self.filters).entries(connection, table)
    }

    /// Forgets the filter history of a connection.
    pub fn clear_filter_history(&self, connection: ConnectionId) {
        lock(&self.filters).clear(connection);
    }

    fn issue_confirmation(&self, key: SaveKey) -> SaveToken {
        let mut pending = lock(&self.pending);
        pending.retain(|p| p.issued.elapsed() < CONFIRMATION_TTL);
        if pending.len() >= MAX_PENDING_CONFIRMATIONS {
            pending.remove(0);
        }
        let id = Uuid::new_v4();
        pending.push(PendingSave {
            id,
            key,
            issued: Instant::now(),
        });
        SaveToken(id)
    }

    fn redeem_confirmation(&self, token: SaveToken, key: &SaveKey) -> bool {
        let mut pending = lock(&self.pending);
        pending.retain(|p| p.issued.elapsed() < CONFIRMATION_TTL);
        match pending.iter().position(|p| p.id == token.0) {
            Some(i) => pending.remove(i).key == *key,
            None => false,
        }
    }
}

/// Tables scanned at most by [`EditService::referencing_foreign_keys`].
pub const MAX_REFERENCING_SCAN: usize = 512;

/// Foreign keys referencing a table ([`EditService::referencing_foreign_keys`]).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ReferencingKeys {
    /// Each referencing table's path with its foreign key.
    pub keys: Vec<(ObjectPath, ninesql_driver_api::ForeignKeyInfo)>,
    /// Every table of the schema was scanned.
    pub complete: bool,
}

async fn referencing(
    metadata: &MetadataService,
    connection: ConnectionId,
    path: ObjectPath,
) -> Result<ReferencingKeys, EditError> {
    let Some((_, parents)) = path.0.split_last() else {
        return Err(NotEditable::NotSingleTable.into());
    };
    let parent = ObjectPath(parents.to_vec());
    let tables = metadata
        .children(connection, NodeKey::new(parent, ObjectKind::Table))
        .await?;
    let mut complete = tables.complete && tables.items.len() <= MAX_REFERENCING_SCAN;
    let mut keys = Vec::new();
    for table in tables.items.iter().take(MAX_REFERENCING_SCAN) {
        let kinds = metadata.child_kinds(connection, &table.path)?;
        if !kinds.contains(&ObjectKind::ForeignKey) {
            continue;
        }
        let fks = match metadata
            .children(
                connection,
                NodeKey::new(table.path.clone(), ObjectKind::ForeignKey),
            )
            .await
        {
            Ok(fks) => fks,
            Err(MetadataError::Driver(_)) => {
                complete = false;
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        complete &= fks.complete;
        for o in &fks.items {
            if let ObjectDetails::ForeignKey(fk) = &o.details
                && fk.referenced_table == path
            {
                keys.push((table.path.clone(), fk.clone()));
            }
        }
    }
    Ok(ReferencingKeys { keys, complete })
}

async fn describe(
    metadata: &MetadataService,
    info: &SessionInfo,
    path: ObjectPath,
) -> Result<EditableTable, EditError> {
    session_editable(info)?;
    let connection = info.connection;
    let Some((_, parents)) = path.0.split_last() else {
        return Err(NotEditable::NotSingleTable.into());
    };
    let leaf_kind = path.kind().unwrap_or(ObjectKind::Table);
    let parent = ObjectPath(parents.to_vec());
    let siblings = metadata
        .children(connection, NodeKey::new(parent, leaf_kind))
        .await?;
    let table_info = siblings
        .items
        .iter()
        .find(|o| o.path == path)
        .and_then(|o| match &o.details {
            ObjectDetails::Table(t) => Some(t.clone()),
            _ => None,
        });
    let kinds = metadata.child_kinds(connection, &path)?;
    if !kinds.contains(&ObjectKind::Column) {
        return Err(NotEditable::NotSingleTable.into());
    }
    let columns = metadata
        .children(connection, NodeKey::new(path.clone(), ObjectKind::Column))
        .await?;
    let indexes = if kinds.contains(&ObjectKind::Index) {
        metadata
            .children(connection, NodeKey::new(path.clone(), ObjectKind::Index))
            .await?
            .items
            .clone()
    } else {
        Vec::new()
    };
    let complete = columns.complete && columns.hidden_by_filter == 0;
    Ok(EditableTable::from_metadata(
        &info.capabilities.engine,
        path,
        table_info.as_ref(),
        &columns.items,
        complete,
        &indexes,
    )?)
}

// ---------------------------------------------------------------------------------------------
// Save task

/// The save's lease and whether its transaction is open. Dropping it while the transaction is
/// open (the task dropped mid-way) queues a `ROLLBACK` before the lease is released.
struct SaveTx {
    lease: SessionLease,
    in_tx: bool,
}

impl Drop for SaveTx {
    fn drop(&mut self) {
        if self.in_tx {
            drop(self.lease.rollback());
        }
    }
}

async fn tx(
    reply: Reply<Result<crate::session::TransactionState, CoreError>>,
) -> Result<(), EditError> {
    match reply.await {
        Some(Ok(_)) => Ok(()),
        Some(Err(e)) => Err(EditError::from_core(e)),
        None => Err(EditError::Core(CoreError::SessionClosed)),
    }
}

struct SaveTask {
    service: EditService,
    session: SessionHandle,
    table: Vec<String>,
    plan: Result<SavePlan, EditError>,
    options: SaveOptions,
    events: mpsc::Sender<SaveEvent>,
    cancel: CancelToken,
}

impl SaveTask {
    async fn run(mut self) {
        let event = self.run_inner().await;
        match &event {
            SaveEvent::Done(s) => tracing::info!(
                session = %self.session.id(),
                statements = s.statements,
                "edits saved"
            ),
            SaveEvent::Failed(e) => tracing::info!(
                session = %self.session.id(),
                kind = e.kind(),
                "edits not saved"
            ),
            _ => {}
        }
        let _ = self.events.send(event).await;
    }

    async fn run_inner(&mut self) -> SaveEvent {
        let started = Instant::now();
        let info = self.session.info().clone();
        if let Err(e) = session_editable(&info) {
            return SaveEvent::Failed(e.into());
        }
        let plan = match std::mem::replace(&mut self.plan, Err(EditError::NothingToSave)) {
            Ok(p) => p,
            Err(e) => return SaveEvent::Failed(e),
        };
        // A statement that is not a filtered write never runs (the P5-B self-check, again with
        // the dialect as it is now).
        let dialect = self.session.dialect();
        if plan
            .batch
            .statements
            .iter()
            .any(|s| dialect.classify(&s.statement.text) != StatementClass::Write)
        {
            return SaveEvent::Failed(EditError::Dml(DmlError::SelfCheck(
                "a statement does not classify as a filtered write",
            )));
        }

        // Confirmation (ADR-0007): one for the whole save.
        let confirmed = if info.policy.production || info.policy.confirm_every_write {
            let key = SaveKey {
                session: info.id,
                table: self.table.clone(),
                statements: plan
                    .batch
                    .statements
                    .iter()
                    .map(|s| (s.statement.text.clone(), s.statement.params.clone()))
                    .collect(),
            };
            match self.options.confirmation {
                None => {
                    let token = self.service.issue_confirmation(key);
                    return SaveEvent::NeedsConfirmation(SaveConfirmation {
                        token,
                        connection_name: info.connection_name.clone(),
                        production: info.policy.production,
                        table: plan.table.clone(),
                        script: plan.script.clone(),
                        statements: plan.batch.statements.len(),
                        deletes: plan.deletes,
                        updates: plan.updates,
                        inserts: plan.inserts,
                        unchecked_columns: plan.unchecked_columns.clone(),
                    });
                }
                Some(token) => {
                    if !self.service.redeem_confirmation(token, &key) {
                        return SaveEvent::Failed(EditError::InvalidConfirmation);
                    }
                    tracing::info!(session = %info.id, "save confirmed");
                    true
                }
            }
        } else {
            false
        };

        let Some(lease) = crate::transfer::acquire_lease(&self.session).await else {
            return SaveEvent::Failed(EditError::SessionBusy);
        };
        // Read with the session idle and leased: nothing else changes it until the end.
        let state = self.session.transaction_state();
        let mode = if state.auto_commit {
            if state.in_transaction {
                return SaveEvent::Failed(EditError::PendingTransaction);
            }
            TxMode::Own
        } else if info.capabilities.transactions.savepoints {
            TxMode::Savepoint {
                opened: !state.in_transaction,
            }
        } else if state.in_transaction {
            return SaveEvent::Failed(EditError::PendingTransaction);
        } else {
            TxMode::Own
        };
        let mut guard = SaveTx {
            lease,
            in_tx: false,
        };
        let total = plan.batch.statements.len();
        let mut savepoint = false;
        let outcome: Result<u64, EditError> = async {
            if matches!(mode, TxMode::Own | TxMode::Savepoint { opened: true }) {
                tx(guard.lease.begin()).await?;
                guard.in_tx = true;
            }
            if matches!(mode, TxMode::Savepoint { .. }) {
                self.control(&guard.lease, &format!("SAVEPOINT {SAVEPOINT}"))
                    .await?;
                savepoint = true;
                // A save dropped mid-way rolls the session's transaction back (the safe side).
                guard.in_tx = true;
            }
            let _ = self
                .events
                .try_send(SaveEvent::Started { statements: total });
            let mut rows = 0u64;
            for (i, statement) in plan.batch.statements.iter().enumerate() {
                let count = self
                    .run_statement(&guard.lease, statement, confirmed)
                    .await?;
                statement
                    .check_affected(count)
                    .map_err(|source| EditError::RowCount {
                        index: i + 1,
                        source,
                    })?;
                rows += count.unwrap_or(0);
                let _ = self
                    .events
                    .try_send(SaveEvent::Progress { done: i + 1, total });
            }
            if self.cancel.is_cancelled() {
                return Err(EditError::Cancelled);
            }
            match mode {
                TxMode::Own => {
                    tx(guard.lease.commit()).await?;
                }
                TxMode::Savepoint { .. } => {
                    self.control(&guard.lease, &format!("RELEASE SAVEPOINT {SAVEPOINT}"))
                        .await?;
                }
            }
            guard.in_tx = false;
            Ok(rows)
        }
        .await;
        match outcome {
            Ok(rows) => SaveEvent::Done(SaveSummary {
                statements: total,
                rows_affected: rows,
                unchecked_columns: plan.unchecked_columns,
                committed: mode == TxMode::Own,
                elapsed: started.elapsed(),
            }),
            Err(e) => {
                guard.in_tx = false;
                let undone = match mode {
                    // The session's own transaction is kept: only the save is undone.
                    // The savepoint was never set: the session's transaction is left as it is.
                    TxMode::Savepoint { opened: false } if !savepoint => Ok(()),
                    TxMode::Savepoint { opened: false } => self
                        .control(&guard.lease, &format!("ROLLBACK TO SAVEPOINT {SAVEPOINT}"))
                        .await
                        .and(
                            self.control(&guard.lease, &format!("RELEASE SAVEPOINT {SAVEPOINT}"))
                                .await,
                        ),
                    _ if self.session.transaction_state().in_transaction => {
                        tx(guard.lease.rollback()).await
                    }
                    _ => Ok(()),
                };
                if let Err(re) = undone {
                    tracing::warn!(
                        kind = re.kind(),
                        "undoing a failed save failed; rolling back"
                    );
                    if let Err(re) = tx(guard.lease.rollback()).await {
                        tracing::warn!(kind = re.kind(), "rollback after a failed save failed");
                    }
                }
                SaveEvent::Failed(e)
            }
        }
    }

    /// Runs a transaction-control statement (`SAVEPOINT`, `RELEASE`, `ROLLBACK TO`) on the
    /// lease, to its end. Not cancellable: it is what undoes a cancelled save.
    async fn control(&self, lease: &SessionLease, text: &str) -> Result<(), EditError> {
        let mut exec = lease.execute(ExecuteRequest::sql(text));
        loop {
            match tokio::time::timeout(CONTROL_TIMEOUT, exec.next()).await {
                Ok(Some(ExecEvent::Finished { .. })) => return Ok(()),
                Ok(Some(ExecEvent::Failed(e))) => return Err(EditError::Core(e)),
                Ok(Some(ExecEvent::NeedsConfirmation(_))) => {
                    return Err(EditError::UnexpectedConfirmation);
                }
                Ok(Some(_)) => {}
                Ok(None) => return Err(EditError::Core(CoreError::SessionClosed)),
                Err(_) => {
                    return Err(EditError::Core(CoreError::Driver(DriverError::Timeout(
                        CONTROL_TIMEOUT,
                    ))));
                }
            }
        }
    }

    /// Runs one statement in its bound form; answers the engine's update count. Answers the
    /// session's per-statement confirmation only for this exact statement of a confirmed save.
    async fn run_statement(
        &self,
        lease: &SessionLease,
        statement: &DmlStatement,
        confirmed: bool,
    ) -> Result<Option<u64>, EditError> {
        let request = ExecuteRequest::sql(statement.statement.text.clone())
            .with_params(statement.statement.params.clone())
            .with_timeout(self.options.statement_timeout);
        match self.execute(lease, request.clone()).await? {
            Ok(count) => Ok(count),
            Err(c) => {
                let expected = confirmed
                    && c.statement == request.text
                    && c.params == request.params.len()
                    && c.class == StatementClass::Write;
                if !expected {
                    return Err(EditError::UnexpectedConfirmation);
                }
                match self.execute(lease, request.confirmed(c.token)).await? {
                    Ok(count) => Ok(count),
                    Err(_) => Err(EditError::UnexpectedConfirmation),
                }
            }
        }
    }

    /// `Ok(Ok(count))` when the statement finished, `Ok(Err(request))` when the session asks
    /// for a confirmation.
    async fn execute(
        &self,
        lease: &SessionLease,
        request: ExecuteRequest,
    ) -> Result<Result<Option<u64>, ConfirmationRequest>, EditError> {
        let mut exec = lease.execute(request);
        let mut count = None;
        loop {
            let event = tokio::select! {
                biased;
                () = self.cancel.cancelled() => {
                    exec.cancel();
                    return Err(EditError::Cancelled);
                }
                event = exec.next() => event,
            };
            match event {
                Some(ExecEvent::Item(ResultItem::UpdateCount(n))) => {
                    count = match (count, n) {
                        (Some(a), Some(b)) => Some(a + b),
                        (None, n) | (n, None) => n,
                    };
                }
                Some(ExecEvent::Finished { .. }) => return Ok(Ok(count)),
                Some(ExecEvent::Failed(e)) => return Err(EditError::from_core(e)),
                Some(ExecEvent::NeedsConfirmation(c)) => return Ok(Err(c)),
                Some(_) => {}
                None => return Err(EditError::Core(CoreError::SessionClosed)),
            }
        }
    }
}
