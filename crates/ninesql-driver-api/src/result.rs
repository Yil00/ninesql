//! Result model (ADR-0004): Arrow batches, documents, update counts, notices and truncation,
//! delivered through a [`ResultStream`] that enforces the memory budget, the timeout and the
//! cancel token for every driver in the same way (I6).
//!
//! # Stream grammar
//! ```text
//! stream   := item* (Truncated | error)?
//! item     := ResultSet (Rows* | Documents*)   -- one result set, header first
//!           | UpdateCount | Notice
//! ```
//! A statement may produce several result sets when
//! [`Capabilities::multiple_result_sets`](crate::Capabilities::multiple_result_sets) is declared.
//! [`ResultItem::Truncated`] and errors are terminal: the stream yields `None` afterwards.
//!
//! # Back-pressure
//! Streams are pull-based. Drivers fetch one batch of
//! [`FetchBudget::batch_rows`](crate::FetchBudget::batch_rows) rows per poll and must not read
//! ahead unboundedly; a consumer that stops polling (the grid showing its first page) pauses the
//! server cursor. Dropping a [`ResultStream`] drops the driver's raw stream, which must release
//! the cursor / portal and leave the session usable.
//!
//! # Timeout while the consumer is not polling (L2)
//! A paused stream still holds a server cursor (and often a transaction snapshot or locks).
//! When [`ExecOptions::timeout`] is set and the stream is created on a Tokio runtime,
//! [`ResultStream::new`] spawns a **watchdog** task that calls the server-side
//! [`CancelHandle`] at the deadline even if nobody polls; the next poll then yields
//! [`DriverError::Timeout`]. The watchdog ends as soon as the stream finishes or is dropped.
//! Without a runtime (synchronous tests) the deadline is only checked while polling.
//!
//! # Cancellation (M6)
//! The [`CancelHandle`] given to [`ResultStream::new`] is scoped to that execution
//! ([`ExecutionTracker::cancel_handle`](crate::ExecutionTracker::cancel_handle)): a late cancel
//! never reaches the next statement. On timeout or cancel the stream calls it once (watchdog
//! and poll path share one claim), then, when the handle
//! [confirms the end](CancelHandle::confirms_end), keeps polling the raw stream (discarding
//! items) until the driver ends it, bounded by [`SERVER_CANCEL_GRACE`]: the driver's
//! [`ExecutionGuard`](crate::ExecutionGuard) lives in the raw stream, so the session stays busy
//! until the server confirmed that the cancelled statement ended.
//!
//! # Integrity checks (L3)
//! The stream fails with [`DriverError::Protocol`] when a driver breaks the grammar: `Rows`
//! without a tabular header or with a schema that differs from the header's beyond field
//! metadata, `Documents` without a document header. A `Truncated` item sent by the driver is
//! terminal like one produced by the budget.

use std::fmt;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use arrow_array::{ArrayRef, RecordBatch, RecordBatchOptions, make_array};
use arrow_data::transform::{Capacities, MutableArrayData};
use arrow_schema::{Schema, SchemaRef};
use futures_core::Stream;
use futures_util::StreamExt;
use tokio::sync::oneshot;

use crate::error::{DriverError, DriverResult};
use crate::execution::ExecutionId;
use crate::statement::{CancelToken, ExecOptions, FetchBudget};

/// Arrow field metadata key holding the engine's original type name when the Arrow type is not
/// a faithful mapping (the column is then `Utf8`, ADR-0004).
pub const ENGINE_TYPE_METADATA_KEY: &str = "ninesql.type";

/// Arrow field metadata key set to `"true"` (in a batch's schema) when at least one value of
/// the column in that batch was cut at
/// [`FetchBudget::max_value_bytes`](crate::FetchBudget::max_value_bytes).
pub const TRUNCATED_VALUES_METADATA_KEY: &str = "ninesql.truncated";

/// Estimated fixed cost of a notice item, added to its text lengths for the budget.
pub const NOTICE_OVERHEAD_BYTES: u64 = 64;

/// Estimated cost of an update-count item for the budget.
pub const UPDATE_COUNT_BYTES: u64 = 16;

/// One item of a result stream.
#[derive(Clone)]
#[non_exhaustive]
pub enum ResultItem {
    /// Starts a result set.
    ResultSet(ResultSetHeader),
    /// Rows of the current tabular result set. The batch schema equals the header's up to field
    /// metadata: a batch may add [`TRUNCATED_VALUES_METADATA_KEY`] to a field. The grid treats
    /// such a column as **non-editable** for that result (writing back a cut value would
    /// destroy data).
    Rows(RecordBatch),
    /// Documents of the current document result set.
    Documents(Vec<Document>),
    /// Rows affected by a DML / DDL statement (`None` when the engine does not report a count).
    UpdateCount(Option<u64>),
    /// Server notice or warning (PostgreSQL `NOTICE`, MySQL `SHOW WARNINGS`…).
    Notice(Notice),
    /// The budget was reached; the result is incomplete. Terminal.
    Truncated(Truncation),
}

impl fmt::Debug for ResultItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never prints result values.
        match self {
            Self::ResultSet(h) => f.debug_tuple("ResultSet").field(h).finish(),
            Self::Rows(b) => write!(f, "Rows({} rows x {} cols)", b.num_rows(), b.num_columns()),
            Self::Documents(d) => write!(f, "Documents({})", d.len()),
            Self::UpdateCount(n) => f.debug_tuple("UpdateCount").field(n).finish(),
            Self::Notice(n) => f.debug_tuple("Notice").field(&n.severity).finish(),
            Self::Truncated(t) => f.debug_tuple("Truncated").field(t).finish(),
        }
    }
}

/// Header of one result set. Build with [`ResultSetHeader::tabular`] /
/// [`ResultSetHeader::documents`].
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct ResultSetHeader {
    /// 0-based index of the result set within the statement.
    pub index: u32,
    /// What the result set contains.
    pub kind: ResultSetKind,
}

/// Estimated fixed cost of a result-set header for the budget, plus [`FIELD_OVERHEAD_BYTES`]
/// and the name / metadata lengths of each field.
pub const HEADER_OVERHEAD_BYTES: u64 = 64;

/// Estimated fixed cost of one field of a tabular header for the budget.
pub const FIELD_OVERHEAD_BYTES: u64 = 64;

impl ResultSetHeader {
    /// Estimated size counted against [`FetchBudget::max_bytes`](crate::FetchBudget::max_bytes)
    /// (F5): a statement returning thousands of empty result sets, or one with thousands of
    /// columns, still consumes the budget.
    #[must_use]
    pub fn estimated_size(&self) -> u64 {
        let fields = match &self.kind {
            ResultSetKind::Tabular(schema) => schema
                .fields()
                .iter()
                .map(|f| {
                    let meta: usize = f.metadata().iter().map(|(k, v)| k.len() + v.len()).sum();
                    FIELD_OVERHEAD_BYTES + (f.name().len() + meta) as u64
                })
                .sum(),
            ResultSetKind::Documents => 0,
        };
        HEADER_OVERHEAD_BYTES + fields
    }

    /// Header of a tabular result set.
    #[must_use]
    pub fn tabular(index: u32, schema: SchemaRef) -> Self {
        Self {
            index,
            kind: ResultSetKind::Tabular(schema),
        }
    }

    /// Header of a document result set.
    #[must_use]
    pub fn documents(index: u32) -> Self {
        Self {
            index,
            kind: ResultSetKind::Documents,
        }
    }
}

/// Content of a result set.
#[derive(Clone, Debug, PartialEq)]
pub enum ResultSetKind {
    /// Arrow batches with this schema. Columns without a faithful Arrow type are `Utf8` with
    /// [`ENGINE_TYPE_METADATA_KEY`] in their field metadata.
    Tabular(SchemaRef),
    /// Documents.
    Documents,
}

/// Severity of a notice.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NoticeSeverity {
    /// Debug / log level.
    Debug,
    /// Informational.
    Info,
    /// Warning.
    Warning,
}

/// A server notice. Its message comes from the engine and is shown in the output panel.
///
/// The message is **display-only**: it may quote data or statement text, so it is never written
/// to logs (logs record the severity and code only) and never for production connections.
/// Notices count against [`FetchBudget::max_bytes`](crate::FetchBudget::max_bytes).
/// Build with [`Notice::new`].
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Notice {
    /// Severity.
    pub severity: NoticeSeverity,
    /// Engine code (SQLSTATE…), if any.
    pub code: Option<String>,
    /// Message text.
    pub message: String,
}

impl Notice {
    /// A notice without an engine code.
    #[must_use]
    pub fn new(severity: NoticeSeverity, message: impl Into<String>) -> Self {
        Self {
            severity,
            code: None,
            message: message.into(),
        }
    }

    /// Adds the engine code.
    #[must_use]
    pub fn with_code(mut self, code: impl Into<String>) -> Self {
        self.code = Some(code.into());
        self
    }
}

/// Why a result stopped early.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TruncationReason {
    /// [`FetchBudget::max_bytes`] reached.
    MaxBytes,
    /// [`FetchBudget::max_rows`] reached.
    MaxRows,
}

/// Payload of [`ResultItem::Truncated`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct Truncation {
    /// Which limit was hit.
    pub reason: TruncationReason,
    /// Rows / documents delivered before truncation (all result sets).
    pub rows_delivered: u64,
    /// Estimated bytes delivered before truncation.
    pub bytes_delivered: u64,
}

impl Truncation {
    /// A truncation marker. Drivers that stop a result themselves (an engine-side row limit
    /// reached) send it as their last item; [`ResultStream`] ends the stream after it.
    #[must_use]
    pub fn new(reason: TruncationReason, rows_delivered: u64, bytes_delivered: u64) -> Self {
        Self {
            reason,
            rows_delivered,
            bytes_delivered,
        }
    }
}

/// An ordered document (MongoDB document, Redis hash, Elasticsearch hit, Neo4j node…).
///
/// `Debug` prints the field count only (result values are never logged). Build with
/// [`Document::new`] or `Document::from(fields)`.
#[derive(Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct Document {
    /// Fields in engine order.
    pub fields: Vec<(String, DocValue)>,
}

impl From<Vec<(String, DocValue)>> for Document {
    fn from(fields: Vec<(String, DocValue)>) -> Self {
        Self { fields }
    }
}

impl Document {
    /// A document with these fields, in engine order.
    #[must_use]
    pub fn new(fields: Vec<(String, DocValue)>) -> Self {
        Self { fields }
    }

    /// `true` when any value, at any depth, is [`DocValue::Truncated`]. The document viewer
    /// then treats the document as **non-editable** (writing back a cut value destroys data).
    #[must_use]
    pub fn has_truncated_values(&self) -> bool {
        self.fields.iter().any(|(_, v)| v.has_truncated_values())
    }

    /// Estimated heap size in bytes, used for the memory budget.
    #[must_use]
    pub fn estimated_size(&self) -> u64 {
        let own = (self.fields.capacity() * std::mem::size_of::<(String, DocValue)>()) as u64;
        own + self
            .fields
            .iter()
            .map(|(k, v)| k.capacity() as u64 + v.estimated_heap_size())
            .sum::<u64>()
    }
}

impl fmt::Debug for Document {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Document({} fields)", self.fields.len())
    }
}

/// A value inside a [`Document`], keeping engine types that JSON would lose.
///
/// `Debug` prints the variant only.
#[derive(Clone, PartialEq)]
#[non_exhaustive]
pub enum DocValue {
    /// Null.
    Null,
    /// Boolean.
    Bool(bool),
    /// Integer.
    Int(i64),
    /// Float.
    Float(f64),
    /// Exact decimal as canonical text (BSON Decimal128…).
    Decimal(String),
    /// String.
    String(String),
    /// Binary.
    Bytes(Vec<u8>),
    /// Date, days since 1970-01-01.
    Date(i32),
    /// UTC timestamp, microseconds since the epoch.
    Timestamp(i64),
    /// BSON ObjectId.
    ObjectId([u8; 12]),
    /// Array.
    Array(Vec<DocValue>),
    /// Nested document.
    Object(Document),
    /// Any other engine type, kept as its type name and text form.
    Other {
        /// Engine type name.
        type_name: String,
        /// Text representation.
        text: String,
    },
    /// A value the driver cut to stay within
    /// [`FetchBudget::max_value_bytes`](crate::FetchBudget::max_value_bytes) /
    /// [`max_row_bytes`](crate::FetchBudget::max_row_bytes) (F4): the prefix of a string or
    /// binary, or an array / object with trailing elements dropped. The document is not
    /// editable ([`Document::has_truncated_values`]). Build with [`DocValue::truncated`].
    Truncated {
        /// What was kept.
        kept: Box<DocValue>,
        /// Size of the original value in bytes (or elements for arrays / objects), when known.
        original_len: Option<u64>,
    },
}

impl DocValue {
    /// Marks `kept` as the cut form of a longer value.
    #[must_use]
    pub fn truncated(kept: DocValue, original_len: Option<u64>) -> Self {
        Self::Truncated {
            kept: Box::new(kept),
            original_len,
        }
    }

    /// `true` for [`DocValue::Truncated`] or a container holding one.
    #[must_use]
    pub fn has_truncated_values(&self) -> bool {
        match self {
            Self::Truncated { .. } => true,
            Self::Array(a) => a.iter().any(Self::has_truncated_values),
            Self::Object(d) => d.has_truncated_values(),
            _ => false,
        }
    }

    fn estimated_heap_size(&self) -> u64 {
        match self {
            Self::Decimal(s) | Self::String(s) => s.capacity() as u64,
            Self::Bytes(b) => b.capacity() as u64,
            Self::Array(a) => {
                (a.capacity() * std::mem::size_of::<DocValue>()) as u64
                    + a.iter().map(Self::estimated_heap_size).sum::<u64>()
            }
            Self::Object(d) => d.estimated_size(),
            Self::Other { type_name, text } => (type_name.capacity() + text.capacity()) as u64,
            Self::Truncated { kept, .. } => {
                std::mem::size_of::<DocValue>() as u64 + kept.estimated_heap_size()
            }
            Self::Null
            | Self::Bool(_)
            | Self::Int(_)
            | Self::Float(_)
            | Self::Date(_)
            | Self::Timestamp(_)
            | Self::ObjectId(_) => 0,
        }
    }

    fn variant(&self) -> &'static str {
        match self {
            Self::Null => "Null",
            Self::Bool(_) => "Bool",
            Self::Int(_) => "Int",
            Self::Float(_) => "Float",
            Self::Decimal(_) => "Decimal",
            Self::String(_) => "String",
            Self::Bytes(_) => "Bytes",
            Self::Date(_) => "Date",
            Self::Timestamp(_) => "Timestamp",
            Self::ObjectId(_) => "ObjectId",
            Self::Array(_) => "Array",
            Self::Object(_) => "Object",
            Self::Other { .. } => "Other",
            Self::Truncated { .. } => "Truncated",
        }
    }
}

impl fmt::Debug for DocValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DocValue::{}", self.variant())
    }
}

/// Stops a running statement on the server. Obtained from
/// [`Session::cancel_handle`](crate::Session::cancel_handle); independent of the session borrow
/// so it can be used while a [`ResultStream`] is being consumed on another task.
#[async_trait::async_trait]
pub trait CancelHandle: Send + Sync {
    /// Asks the server to stop the statement (cancel request, `KILL QUERY`, interrupt…).
    /// Engines with [`CancelSupport::LocalOnly`](crate::CancelSupport::LocalOnly) return `Ok(())`
    /// without network activity. Must not fail when nothing is running.
    ///
    /// Handles given out by a session are **scoped to one execution** (M6): once that
    /// execution ended, `cancel` does nothing, so a late cancel never reaches the next
    /// statement. Build them with
    /// [`ExecutionTracker::cancel_handle`](crate::ExecutionTracker::cancel_handle).
    async fn cancel(&self) -> DriverResult<()>;

    /// `true` when, after [`cancel`](Self::cancel), the driver's raw stream ends by itself once
    /// the server confirmed that the statement stopped (PostgreSQL: the `57014` error and
    /// `ReadyForQuery`; MySQL: the `KILL QUERY` error packet). [`ResultStream`] then drains the
    /// raw stream, bounded by [`SERVER_CANCEL_GRACE`], instead of dropping it at once, so the
    /// session stays busy until that confirmation. Default `false` (dropped at once): drivers
    /// with [`CancelSupport::ServerSide`](crate::CancelSupport::ServerSide) override it.
    fn confirms_end(&self) -> bool {
        false
    }
}

/// A [`CancelHandle`] that does nothing, for engines without server-side cancel.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoServerCancel;

#[async_trait::async_trait]
impl CancelHandle for NoServerCancel {
    async fn cancel(&self) -> DriverResult<()> {
        Ok(())
    }
}

/// The driver-produced stream, before budget / timeout / cancel enforcement.
pub type RawResultStream = Pin<Box<dyn Stream<Item = DriverResult<ResultItem>> + Send>>;

/// Upper bound on how long the stream waits for a server-side cancel to be acknowledged (and,
/// for handles that [confirm the end](CancelHandle::confirms_end), for the raw stream to end)
/// before reporting the timeout / cancellation anyway.
pub const SERVER_CANCEL_GRACE: Duration = Duration::from_secs(5);

/// The result set the stream is in, for the integrity checks.
enum CurrentSet {
    Tabular(SchemaRef),
    Documents,
}

/// Result of [`Session::execute`](crate::Session::execute).
///
/// Wraps the driver's [`RawResultStream`] and enforces, uniformly for every driver:
/// - the [`FetchBudget`]: rows and estimated bytes are counted (headers, batches, documents,
///   notices, update counts); the item that would cross a limit is cut and followed by
///   [`ResultItem::Truncated`]; the raw stream is then dropped. A cut Arrow batch is
///   **copied** (copy-on-slice), so the buffers of the dropped rows are released and the bytes
///   counted are the bytes retained (L3);
/// - the timeout of [`ExecOptions::timeout`], measured from the creation of the stream, also
///   while the consumer is not polling (watchdog, see the [module documentation](self));
/// - the [`CancelToken`] of [`ExecOptions::cancel`];
/// - the stream grammar (module documentation): a malformed stream fails with
///   [`DriverError::Protocol`].
///
/// On timeout or cancel the execution-scoped [`CancelHandle`] is called (bounded by
/// [`SERVER_CANCEL_GRACE`]), the raw stream is drained until the server confirms the end (when
/// the handle [confirms it](CancelHandle::confirms_end)) or dropped, and the stream yields
/// [`DriverError::Timeout`] or [`DriverError::Cancelled`].
///
/// Drivers construct it with [`ResultStream::new`] as early as possible in `execute`, with a lazy
/// raw stream that does the network work when polled, so the timeout and cancel cover it.
pub struct ResultStream {
    inner: Option<RawResultStream>,
    budget: FetchBudget,
    timeout: Option<Duration>,
    deadline: Option<tokio::time::Instant>,
    cancel: CancelToken,
    canceller: Arc<dyn CancelHandle>,
    /// Set by whoever calls the server-side cancel first (watchdog or poll path).
    cancel_claimed: Arc<AtomicBool>,
    /// Dropping it stops the watchdog task.
    watchdog: Option<oneshot::Sender<()>>,
    execution: Option<ExecutionId>,
    current: Option<CurrentSet>,
    /// The raw stream was polled at least once (something may have reached the server).
    polled: bool,
    rows: u64,
    bytes: u64,
}

impl fmt::Debug for ResultStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResultStream")
            .field("finished", &self.inner.is_none())
            .field("execution", &self.execution)
            .field("budget", &self.budget)
            .field("timeout", &self.timeout)
            .field("rows_delivered", &self.rows)
            .field("bytes_delivered", &self.bytes)
            .finish_non_exhaustive()
    }
}

impl Drop for ResultStream {
    fn drop(&mut self) {
        // Stop the watchdog before the raw stream goes away.
        self.watchdog.take();
    }
}

impl ResultStream {
    /// Wraps a driver stream with the options of the `execute` call. `canceller` must be scoped
    /// to this execution ([`ExecutionTracker::cancel_handle`](crate::ExecutionTracker::cancel_handle)).
    ///
    /// When `options.timeout` is set and a Tokio runtime is current, spawns the timeout
    /// watchdog (module documentation).
    #[must_use]
    pub fn new(
        raw: RawResultStream,
        options: &ExecOptions,
        canceller: Arc<dyn CancelHandle>,
    ) -> Self {
        let deadline = options.timeout.map(|t| tokio::time::Instant::now() + t);
        let cancel_claimed = Arc::new(AtomicBool::new(false));
        let watchdog = deadline
            .and_then(|d| spawn_watchdog(d, Arc::clone(&canceller), Arc::clone(&cancel_claimed)));
        Self {
            inner: Some(raw),
            budget: options.budget,
            timeout: options.timeout,
            deadline,
            cancel: options.cancel.clone(),
            canceller,
            cancel_claimed,
            watchdog,
            execution: None,
            current: None,
            polled: false,
            rows: 0,
            bytes: 0,
        }
    }

    /// Records the execution this stream belongs to ([`ExecutionGuard::id`](crate::ExecutionGuard::id)).
    #[must_use]
    pub fn with_execution(mut self, id: ExecutionId) -> Self {
        self.execution = Some(id);
        self
    }

    /// A stream that yields the given items (subject to the same enforcement). Handy for
    /// statements answered locally and for tests.
    #[must_use]
    pub fn from_items(items: Vec<DriverResult<ResultItem>>, options: &ExecOptions) -> Self {
        Self::new(
            Box::pin(futures_util::stream::iter(items)),
            options,
            Arc::new(NoServerCancel),
        )
    }

    /// The execution this stream belongs to, when the driver recorded it.
    #[must_use]
    pub fn execution_id(&self) -> Option<ExecutionId> {
        self.execution
    }

    /// The server-side cancel handle of **this** execution, usable from another task. It does
    /// nothing once the execution ended, so holding on to it is harmless (M6).
    #[must_use]
    pub fn cancel_handle(&self) -> Arc<dyn CancelHandle> {
        Arc::clone(&self.canceller)
    }

    /// Rows / documents delivered so far.
    #[must_use]
    pub fn rows_delivered(&self) -> u64 {
        self.rows
    }

    /// Estimated bytes delivered so far.
    #[must_use]
    pub fn bytes_delivered(&self) -> u64 {
        self.bytes
    }

    /// `true` once the stream has ended (normally, by truncation, error, timeout or cancel).
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.inner.is_none()
    }

    /// Next item, or `None` at the end. Cancel-safe: dropping the future loses no item.
    pub async fn next(&mut self) -> Option<DriverResult<ResultItem>> {
        let inner = self.inner.as_mut()?;
        if self.cancel.is_cancelled() {
            return Some(Err(self.abort(DriverError::Cancelled).await));
        }
        let deadline = self.deadline;
        let cancel = self.cancel.clone();
        self.polled = true;
        let polled = tokio::select! {
            biased;
            () = cancel.cancelled() => Polled::Cancelled,
            () = sleep_until_opt(deadline) => Polled::TimedOut,
            item = inner.next() => Polled::Item(item),
        };
        match polled {
            Polled::Cancelled => Some(Err(self.abort(DriverError::Cancelled).await)),
            Polled::TimedOut => {
                let t = self.timeout.unwrap_or_default();
                Some(Err(self.abort(DriverError::Timeout(t)).await))
            }
            Polled::Item(None) => {
                self.finish();
                None
            }
            Polled::Item(Some(Err(e))) => {
                self.finish();
                Some(Err(e))
            }
            Polled::Item(Some(Ok(item))) => match self.account(item) {
                Ok(item) => {
                    if matches!(item, ResultItem::Truncated(_)) {
                        self.finish();
                    }
                    Some(Ok(item))
                }
                Err(e) => {
                    tracing::warn!(error = %e, "driver broke the result stream grammar");
                    self.finish();
                    Some(Err(e))
                }
            },
        }
    }

    /// Converts into a [`Stream`], for consumers that prefer combinators.
    pub fn into_stream(self) -> impl Stream<Item = DriverResult<ResultItem>> + Send {
        futures_util::stream::unfold(self, |mut s| async move {
            let item = s.next().await?;
            Some((item, s))
        })
    }

    fn finish(&mut self) {
        self.inner = None;
        self.watchdog.take();
    }

    async fn abort(&mut self, err: DriverError) -> DriverError {
        self.watchdog.take();
        tracing::debug!(
            reason = ?err,
            rows = self.rows,
            execution = self.execution.map(ExecutionId::get),
            "result stream aborted, requesting server-side cancel"
        );
        let started = tokio::time::Instant::now();
        if !self.cancel_claimed.swap(true, Ordering::SeqCst) {
            match tokio::time::timeout(SERVER_CANCEL_GRACE, self.canceller.cancel()).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => tracing::warn!(error = %e, "server-side cancel failed"),
                Err(_) => tracing::warn!("server-side cancel not acknowledged in time"),
            }
        }
        // Wait for the server to confirm the end of the statement: the driver ends its raw
        // stream then, and its execution guard frees the session. Nothing was sent if the raw
        // stream was never polled, so there is nothing to drain.
        if self.polled && self.canceller.confirms_end() {
            let remaining = SERVER_CANCEL_GRACE.saturating_sub(started.elapsed());
            if let Some(inner) = self.inner.as_mut() {
                let drain = async {
                    while let Some(item) = inner.next().await {
                        if item.is_err() {
                            break;
                        }
                    }
                };
                if tokio::time::timeout(remaining, drain).await.is_err() {
                    tracing::warn!(
                        "server did not confirm the end of the cancelled statement in time"
                    );
                }
            }
        }
        self.inner = None;
        err
    }

    fn truncate(&mut self, reason: TruncationReason) -> ResultItem {
        tracing::debug!(
            ?reason,
            rows = self.rows,
            bytes = self.bytes,
            "result truncated"
        );
        self.finish();
        ResultItem::Truncated(Truncation {
            reason,
            rows_delivered: self.rows,
            bytes_delivered: self.bytes,
        })
    }

    fn remaining_rows(&self) -> u64 {
        self.budget
            .max_rows
            .map_or(u64::MAX, |m| m.saturating_sub(self.rows))
    }

    fn remaining_bytes(&self) -> u64 {
        self.budget.max_bytes.saturating_sub(self.bytes)
    }

    /// Applies the grammar checks and the budget to one item. Returns the item to deliver now;
    /// when it had to be cut, the cut part is delivered and `Truncated` is queued as the only
    /// remaining item.
    fn account(&mut self, item: ResultItem) -> DriverResult<ResultItem> {
        Ok(match item {
            ResultItem::ResultSet(header) => {
                let size = header.estimated_size();
                if size > self.remaining_bytes() {
                    return Ok(self.truncate(TruncationReason::MaxBytes));
                }
                self.bytes += size;
                self.current = Some(match &header.kind {
                    ResultSetKind::Tabular(schema) => CurrentSet::Tabular(Arc::clone(schema)),
                    ResultSetKind::Documents => CurrentSet::Documents,
                });
                ResultItem::ResultSet(header)
            }
            ResultItem::Rows(batch) => {
                match &self.current {
                    Some(CurrentSet::Tabular(schema)) => {
                        if !same_schema_up_to_metadata(schema, &batch.schema()) {
                            return Err(DriverError::Protocol(
                                "a batch schema differs from its result set header".into(),
                            ));
                        }
                    }
                    _ => {
                        return Err(DriverError::Protocol(
                            "rows received outside a tabular result set".into(),
                        ));
                    }
                }
                self.account_rows(batch)
            }
            ResultItem::Documents(docs) => {
                if !matches!(self.current, Some(CurrentSet::Documents)) {
                    return Err(DriverError::Protocol(
                        "documents received outside a document result set".into(),
                    ));
                }
                self.account_documents(docs)
            }
            ResultItem::Notice(n) => {
                let size = NOTICE_OVERHEAD_BYTES
                    + n.message.len() as u64
                    + n.code.as_ref().map_or(0, |c| c.len() as u64);
                self.account_small(size, ResultItem::Notice(n))
            }
            ResultItem::UpdateCount(c) => {
                self.account_small(UPDATE_COUNT_BYTES, ResultItem::UpdateCount(c))
            }
            // A driver-sent `Truncated` passes through; `next` ends the stream after it.
            other @ ResultItem::Truncated(_) => other,
        })
    }

    fn account_rows(&mut self, batch: RecordBatch) -> ResultItem {
        let n = batch.num_rows();
        if n == 0 {
            return ResultItem::Rows(batch);
        }
        let size = batch.get_array_memory_size() as u64;
        let (mut keep, reason) = self.fit(n as u64, size);
        if keep == n as u64 {
            self.rows += keep;
            self.bytes += size;
            return ResultItem::Rows(batch);
        }
        // Copy-on-slice: a slice would keep the whole batch's buffers alive while only the
        // slice is counted. Compact the kept rows and count what is actually retained; shrink
        // again if the compacted estimate still does not fit.
        let remaining = self.remaining_bytes();
        for _ in 0..4 {
            if keep == 0 {
                break;
            }
            let k = usize::try_from(keep).unwrap_or(usize::MAX);
            let part = match compact_prefix(&batch, k) {
                Some(part) => part,
                None if size <= remaining => batch.slice(0, k),
                // Cannot compact this type and the whole buffer does not fit: deliver nothing.
                None => break,
            };
            let part_size = part.get_array_memory_size() as u64;
            if part_size <= remaining {
                self.rows += keep;
                self.bytes += part_size;
                return self.pending_truncation(reason, ResultItem::Rows(part));
            }
            keep = keep.saturating_mul(remaining) / part_size.max(1);
        }
        self.truncate(TruncationReason::MaxBytes)
    }

    fn account_documents(&mut self, mut docs: Vec<Document>) -> ResultItem {
        let mut keep = 0usize;
        let mut kept_bytes = 0u64;
        let mut reason = None;
        for d in &docs {
            if keep as u64 >= self.remaining_rows() {
                reason = Some(TruncationReason::MaxRows);
                break;
            }
            let s = d.estimated_size();
            if kept_bytes + s > self.remaining_bytes() {
                reason = Some(TruncationReason::MaxBytes);
                break;
            }
            kept_bytes += s;
            keep += 1;
        }
        self.rows += keep as u64;
        self.bytes += kept_bytes;
        match reason {
            None => ResultItem::Documents(docs),
            Some(r) if keep == 0 => self.truncate(r),
            Some(r) => {
                docs.truncate(keep);
                docs.shrink_to_fit();
                self.pending_truncation(r, ResultItem::Documents(docs))
            }
        }
    }

    /// Accounts an indivisible item; truncates instead of delivering it when it does not fit.
    fn account_small(&mut self, size: u64, item: ResultItem) -> ResultItem {
        if size > self.remaining_bytes() {
            return self.truncate(TruncationReason::MaxBytes);
        }
        self.bytes += size;
        item
    }

    /// How many of `n` rows of estimated total `size` fit, and which limit cuts them.
    fn fit(&self, n: u64, size: u64) -> (u64, TruncationReason) {
        let by_rows = self.remaining_rows().min(n);
        let by_bytes = if size <= self.remaining_bytes() {
            n
        } else {
            // First estimate: rows share the batch's buffers evenly; refined after compaction.
            let per_row = size.div_ceil(n).max(1);
            self.remaining_bytes() / per_row
        };
        if by_bytes < by_rows {
            (by_bytes, TruncationReason::MaxBytes)
        } else {
            (by_rows, TruncationReason::MaxRows)
        }
    }

    fn pending_truncation(&mut self, reason: TruncationReason, part: ResultItem) -> ResultItem {
        let truncated = self.truncate(reason);
        // Deliver the part now, then the truncation marker, then the end. The queued marker is
        // passed through unchanged by `account`, and `next` ends the stream after it.
        self.inner = Some(Box::pin(futures_util::stream::iter([Ok(truncated)])));
        part
    }
}

/// `true` when the batch has the header's fields (names, types, nullability), ignoring field
/// and schema metadata (a batch may add [`TRUNCATED_VALUES_METADATA_KEY`]).
fn same_schema_up_to_metadata(header: &Schema, batch: &Schema) -> bool {
    header.fields().len() == batch.fields().len()
        && header
            .fields()
            .iter()
            .zip(batch.fields().iter())
            .all(|(h, b)| {
                h.name() == b.name()
                    && h.is_nullable() == b.is_nullable()
                    && h.data_type().equals_datatype(b.data_type())
            })
}

/// A compact copy of the first `len` rows (fresh buffers), or `None` when a column type cannot
/// be copied generically.
fn compact_prefix(batch: &RecordBatch, len: usize) -> Option<RecordBatch> {
    let mut columns: Vec<ArrayRef> = Vec::with_capacity(batch.num_columns());
    for column in batch.columns() {
        let data = column.to_data();
        let mut copy =
            MutableArrayData::try_with_capacities(vec![&data], false, Capacities::Array(len))
                .ok()?;
        copy.try_extend(0, 0, len).ok()?;
        columns.push(make_array(copy.freeze()));
    }
    let options = RecordBatchOptions::new().with_row_count(Some(len));
    RecordBatch::try_new_with_options(batch.schema(), columns, &options).ok()
}

fn spawn_watchdog(
    deadline: tokio::time::Instant,
    canceller: Arc<dyn CancelHandle>,
    claimed: Arc<AtomicBool>,
) -> Option<oneshot::Sender<()>> {
    let handle = tokio::runtime::Handle::try_current().ok()?;
    let (stop, stopped) = oneshot::channel::<()>();
    handle.spawn(async move {
        tokio::select! {
            _ = stopped => {}
            () = tokio::time::sleep_until(deadline) => {
                if !claimed.swap(true, Ordering::SeqCst) {
                    tracing::debug!("timeout reached; watchdog requesting server-side cancel");
                    match tokio::time::timeout(SERVER_CANCEL_GRACE, canceller.cancel()).await {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => tracing::warn!(error = %e, "server-side cancel failed"),
                        Err(_) => tracing::warn!("server-side cancel not acknowledged in time"),
                    }
                }
            }
        }
    });
    Some(stop)
}

enum Polled {
    Cancelled,
    TimedOut,
    Item(Option<DriverResult<ResultItem>>),
}

async fn sleep_until_opt(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(d).await,
        None => std::future::pending::<()>().await,
    }
}
