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

use std::fmt;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use arrow_array::RecordBatch;
use arrow_schema::SchemaRef;
use futures_core::Stream;
use futures_util::StreamExt;

use crate::error::{DriverError, DriverResult};
use crate::statement::{CancelToken, ExecOptions, FetchBudget};

/// Arrow field metadata key holding the engine's original type name when the Arrow type is not
/// a faithful mapping (the column is then `Utf8`, ADR-0004).
pub const ENGINE_TYPE_METADATA_KEY: &str = "ninesql.type";

/// One item of a result stream.
#[derive(Clone)]
#[non_exhaustive]
pub enum ResultItem {
    /// Starts a result set.
    ResultSet(ResultSetHeader),
    /// Rows of the current tabular result set; the schema equals the header's.
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

/// Header of one result set.
#[derive(Clone, Debug, PartialEq)]
pub struct ResultSetHeader {
    /// 0-based index of the result set within the statement.
    pub index: u32,
    /// What the result set contains.
    pub kind: ResultSetKind,
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    /// Severity.
    pub severity: NoticeSeverity,
    /// Engine code (SQLSTATE…), if any.
    pub code: Option<String>,
    /// Message text.
    pub message: String,
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
pub struct Truncation {
    /// Which limit was hit.
    pub reason: TruncationReason,
    /// Rows / documents delivered before truncation (all result sets).
    pub rows_delivered: u64,
    /// Estimated bytes delivered before truncation.
    pub bytes_delivered: u64,
}

/// An ordered document (MongoDB document, Redis hash, Elasticsearch hit, Neo4j node…).
///
/// `Debug` prints the field count only (result values are never logged).
#[derive(Clone, Default, PartialEq)]
pub struct Document {
    /// Fields in engine order.
    pub fields: Vec<(String, DocValue)>,
}

impl Document {
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
}

impl DocValue {
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
    /// Asks the server to stop the current statement (cancel request, `KILL QUERY`, interrupt…).
    /// Engines with [`CancelSupport::LocalOnly`](crate::CancelSupport::LocalOnly) return `Ok(())`
    /// without network activity. Must not fail when nothing is running.
    async fn cancel(&self) -> DriverResult<()>;
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

/// Upper bound on how long the stream waits for a server-side cancel to be acknowledged before
/// reporting the timeout / cancellation anyway.
pub const SERVER_CANCEL_GRACE: Duration = Duration::from_secs(5);

/// Result of [`Session::execute`](crate::Session::execute).
///
/// Wraps the driver's [`RawResultStream`] and enforces, uniformly for every driver:
/// - the [`FetchBudget`]: rows and estimated bytes are counted; the item that would cross a limit
///   is cut (Arrow batches are sliced, document lists shortened) and followed by
///   [`ResultItem::Truncated`]; the raw stream is then dropped;
/// - the timeout of [`ExecOptions::timeout`], measured from the creation of the stream;
/// - the [`CancelToken`] of [`ExecOptions::cancel`].
///
/// On timeout or cancel the server-side [`CancelHandle`] is called (bounded by
/// [`SERVER_CANCEL_GRACE`]), the raw stream is dropped, and the stream yields
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
    rows: u64,
    bytes: u64,
}

impl fmt::Debug for ResultStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResultStream")
            .field("finished", &self.inner.is_none())
            .field("budget", &self.budget)
            .field("timeout", &self.timeout)
            .field("rows_delivered", &self.rows)
            .field("bytes_delivered", &self.bytes)
            .finish_non_exhaustive()
    }
}

impl ResultStream {
    /// Wraps a driver stream with the options of the `execute` call.
    #[must_use]
    pub fn new(
        raw: RawResultStream,
        options: &ExecOptions,
        canceller: Arc<dyn CancelHandle>,
    ) -> Self {
        Self {
            inner: Some(raw),
            budget: options.budget,
            timeout: options.timeout,
            deadline: options.timeout.map(|t| tokio::time::Instant::now() + t),
            cancel: options.cancel.clone(),
            canceller,
            rows: 0,
            bytes: 0,
        }
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
                self.inner = None;
                None
            }
            Polled::Item(Some(Err(e))) => {
                self.inner = None;
                Some(Err(e))
            }
            Polled::Item(Some(Ok(item))) => Some(Ok(self.account(item))),
        }
    }

    /// Converts into a [`Stream`], for consumers that prefer combinators.
    pub fn into_stream(self) -> impl Stream<Item = DriverResult<ResultItem>> + Send {
        futures_util::stream::unfold(self, |mut s| async move {
            let item = s.next().await?;
            Some((item, s))
        })
    }

    async fn abort(&mut self, err: DriverError) -> DriverError {
        tracing::debug!(
            reason = ?err,
            rows = self.rows,
            "result stream aborted, requesting server-side cancel"
        );
        match tokio::time::timeout(SERVER_CANCEL_GRACE, self.canceller.cancel()).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::warn!(error = %e, "server-side cancel failed"),
            Err(_) => tracing::warn!("server-side cancel not acknowledged in time"),
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
        self.inner = None;
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

    /// Applies the budget to one item. Returns the item to deliver now; when it had to be cut,
    /// the cut part is delivered and `Truncated` is queued as the only remaining item.
    fn account(&mut self, item: ResultItem) -> ResultItem {
        match item {
            ResultItem::Rows(batch) => {
                let n = batch.num_rows();
                if n == 0 {
                    return ResultItem::Rows(batch);
                }
                let size = batch.get_array_memory_size() as u64;
                let (keep, reason) = self.fit(n as u64, size);
                if keep == n as u64 {
                    self.rows += keep;
                    self.bytes += size;
                    return ResultItem::Rows(batch);
                }
                self.deliver_then_truncate(keep, size, n as u64, reason, |k| {
                    ResultItem::Rows(batch.slice(0, k))
                })
            }
            ResultItem::Documents(mut docs) => {
                let sizes: Vec<u64> = docs.iter().map(Document::estimated_size).collect();
                let mut keep = 0usize;
                let mut kept_bytes = 0u64;
                let mut reason = None;
                for s in &sizes {
                    if keep as u64 >= self.remaining_rows() {
                        reason = Some(TruncationReason::MaxRows);
                        break;
                    }
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
                    Some(r) => {
                        docs.truncate(keep);
                        self.pending_truncation(r, ResultItem::Documents(docs), keep == 0)
                    }
                }
            }
            other => other,
        }
    }

    /// How many of `n` rows of estimated total `size` fit, and which limit cuts them.
    fn fit(&self, n: u64, size: u64) -> (u64, TruncationReason) {
        let by_rows = self.remaining_rows().min(n);
        let by_bytes = if size <= self.remaining_bytes() {
            n
        } else {
            // Proportional estimate: rows share the batch's buffers evenly.
            let per_row = size.div_ceil(n).max(1);
            self.remaining_bytes() / per_row
        };
        if by_bytes < by_rows {
            (by_bytes, TruncationReason::MaxBytes)
        } else {
            (by_rows, TruncationReason::MaxRows)
        }
    }

    fn deliver_then_truncate(
        &mut self,
        keep: u64,
        size: u64,
        n: u64,
        reason: TruncationReason,
        cut: impl FnOnce(usize) -> ResultItem,
    ) -> ResultItem {
        let keep_usize = usize::try_from(keep).unwrap_or(usize::MAX);
        self.rows += keep;
        self.bytes += size.saturating_mul(keep) / n;
        let part = cut(keep_usize);
        self.pending_truncation(reason, part, keep == 0)
    }

    fn pending_truncation(
        &mut self,
        reason: TruncationReason,
        part: ResultItem,
        part_is_empty: bool,
    ) -> ResultItem {
        let truncated = self.truncate(reason);
        if part_is_empty {
            return truncated;
        }
        // Deliver the part now, then the truncation marker, then the end.
        self.inner = Some(Box::pin(futures_util::stream::iter([Ok(truncated)])));
        // The queued marker must not be accounted again, and the stream must stop after it:
        // `account` passes `Truncated` through unchanged, and the one-item stream then ends.
        part
    }
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
