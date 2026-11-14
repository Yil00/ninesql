//! Statements and execution options.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::Notify;

use crate::capabilities::QueryLanguage;

/// A bind parameter value. Its `Debug` output names the type only: parameter values are never
/// logged (AGENTS.md).
#[derive(Clone, PartialEq)]
#[non_exhaustive]
pub enum Param {
    /// SQL `NULL`.
    Null,
    /// Boolean.
    Bool(bool),
    /// 64-bit integer.
    Int(i64),
    /// 64-bit float.
    Float(f64),
    /// Exact decimal, as its canonical text (`"-12.50"`).
    Decimal(String),
    /// Text.
    Text(String),
    /// Binary.
    Bytes(Vec<u8>),
    /// Date, days since 1970-01-01.
    Date(i32),
    /// Timestamp, microseconds since 1970-01-01T00:00:00 (UTC when `utc` is true).
    Timestamp {
        /// Microseconds since the epoch.
        micros: i64,
        /// The value is in UTC (`timestamptz`) rather than a local wall-clock time.
        utc: bool,
    },
    /// JSON document as text.
    Json(String),
}

impl Param {
    fn type_name(&self) -> &'static str {
        match self {
            Self::Null => "Null",
            Self::Bool(_) => "Bool",
            Self::Int(_) => "Int",
            Self::Float(_) => "Float",
            Self::Decimal(_) => "Decimal",
            Self::Text(_) => "Text",
            Self::Bytes(_) => "Bytes",
            Self::Date(_) => "Date",
            Self::Timestamp { .. } => "Timestamp",
            Self::Json(_) => "Json",
        }
    }
}

impl fmt::Debug for Param {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Param::{}(<redacted>)", self.type_name())
    }
}

/// One statement to execute.
///
/// `Debug` prints the language, the text length and the parameter types, never the text or the
/// values: query text may be logged only at `trace` level and never for production connections,
/// which is the caller's decision, not an accident of `{:?}`.
///
/// Build with [`Statement::sql`] / [`Statement::new`] and [`Statement::with_params`].
#[derive(Clone, PartialEq)]
#[non_exhaustive]
pub struct Statement {
    /// Statement text (one statement; scripts are split by the [`Dialect`](crate::Dialect)).
    pub text: String,
    /// Bind parameters, in order. Allowed only when
    /// [`Capabilities::parameters`](crate::Capabilities::parameters) is declared.
    pub params: Vec<Param>,
    /// Language of `text`.
    pub language: QueryLanguage,
}

impl Statement {
    /// A statement in `language`, without parameters.
    #[must_use]
    pub fn new(text: impl Into<String>, language: QueryLanguage) -> Self {
        Self {
            text: text.into(),
            params: Vec::new(),
            language,
        }
    }

    /// A SQL statement without parameters.
    #[must_use]
    pub fn sql(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            params: Vec::new(),
            language: QueryLanguage::Sql,
        }
    }

    /// Adds bind parameters.
    #[must_use]
    pub fn with_params(mut self, params: Vec<Param>) -> Self {
        self.params = params;
        self
    }
}

impl fmt::Debug for Statement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Statement")
            .field("language", &self.language)
            .field("text_len", &self.text.len())
            .field("params", &self.params)
            .finish()
    }
}

/// Memory budget for one result (I6, ADR-0004).
///
/// The stream never delivers more than `max_bytes` (estimated from Arrow buffer sizes, document
/// sizes, notice and update-count items) nor more than `max_rows` rows; it ends with
/// [`ResultItem::Truncated`](crate::ResultItem::Truncated) instead.
///
/// Drivers bound what they build **before** handing it to the stream:
/// - a batch ends at [`effective_batch_rows`](Self::effective_batch_rows) rows or as soon as it
///   reaches [`effective_max_batch_bytes`](Self::effective_max_batch_bytes) (use
///   [`BatchLimiter`]), which also bounds how far the server cursor runs ahead of the consumer;
/// - a single value longer than `max_value_bytes` (text, binary, JSON, LOB) is cut (use
///   [`truncate_str`] / [`truncate_bytes`]) and its field carries
///   [`TRUNCATED_VALUES_METADATA_KEY`](crate::TRUNCATED_VALUES_METADATA_KEY) `= "true"` in the
///   batch schema; drivers stream LOBs instead of reading them whole when the client allows.
///
/// Non-exhaustive: build from `FetchBudget::default()` and the `with_*` methods.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct FetchBudget {
    /// Rows per batch fetched from the server (DBeaver's "fetch size"); clamped to
    /// `1..=MAX_BATCH_ROWS` by [`effective_batch_rows`](Self::effective_batch_rows).
    pub batch_rows: u32,
    /// Maximum bytes held by this result.
    pub max_bytes: u64,
    /// Maximum rows / documents delivered, if any.
    pub max_rows: Option<u64>,
    /// Maximum estimated bytes of one batch.
    pub max_batch_bytes: u64,
    /// Maximum bytes of one value; longer values are cut and flagged.
    pub max_value_bytes: u64,
}

impl FetchBudget {
    /// DBeaver's default fetch size.
    pub const DEFAULT_BATCH_ROWS: u32 = 200;
    /// Upper bound of `batch_rows`.
    pub const MAX_BATCH_ROWS: u32 = 10_000;
    /// Default per-result memory budget: 256 MiB.
    pub const DEFAULT_MAX_BYTES: u64 = 256 * 1024 * 1024;
    /// Default per-batch budget: 8 MiB.
    pub const DEFAULT_MAX_BATCH_BYTES: u64 = 8 * 1024 * 1024;
    /// Default per-value budget: 1 MiB.
    pub const DEFAULT_MAX_VALUE_BYTES: u64 = 1024 * 1024;

    /// Rows per batch, clamped to `1..=MAX_BATCH_ROWS`.
    #[must_use]
    pub fn effective_batch_rows(&self) -> u32 {
        self.batch_rows.clamp(1, Self::MAX_BATCH_ROWS)
    }

    /// Bytes per batch, never above `max_bytes`, at least 1.
    #[must_use]
    pub fn effective_max_batch_bytes(&self) -> u64 {
        self.max_batch_bytes.min(self.max_bytes).max(1)
    }

    /// Bytes per value, never above the batch budget.
    #[must_use]
    pub fn effective_max_value_bytes(&self) -> u64 {
        self.max_value_bytes.min(self.effective_max_batch_bytes())
    }

    /// Sets `batch_rows`.
    #[must_use]
    pub fn with_batch_rows(mut self, rows: u32) -> Self {
        self.batch_rows = rows;
        self
    }

    /// Sets `max_bytes`.
    #[must_use]
    pub fn with_max_bytes(mut self, bytes: u64) -> Self {
        self.max_bytes = bytes;
        self
    }

    /// Sets `max_rows`.
    #[must_use]
    pub fn with_max_rows(mut self, rows: Option<u64>) -> Self {
        self.max_rows = rows;
        self
    }

    /// Sets `max_batch_bytes`.
    #[must_use]
    pub fn with_max_batch_bytes(mut self, bytes: u64) -> Self {
        self.max_batch_bytes = bytes;
        self
    }

    /// Sets `max_value_bytes`.
    #[must_use]
    pub fn with_max_value_bytes(mut self, bytes: u64) -> Self {
        self.max_value_bytes = bytes;
        self
    }
}

impl Default for FetchBudget {
    fn default() -> Self {
        Self {
            batch_rows: Self::DEFAULT_BATCH_ROWS,
            max_bytes: Self::DEFAULT_MAX_BYTES,
            max_rows: None,
            max_batch_bytes: Self::DEFAULT_MAX_BATCH_BYTES,
            max_value_bytes: Self::DEFAULT_MAX_VALUE_BYTES,
        }
    }
}

/// Helps a driver end a batch at the budget's row and byte limits while it decodes rows.
#[derive(Clone, Copy, Debug)]
pub struct BatchLimiter {
    max_rows: u32,
    max_bytes: u64,
    rows: u32,
    bytes: u64,
}

impl BatchLimiter {
    /// A limiter for one batch under `budget`.
    #[must_use]
    pub fn new(budget: &FetchBudget) -> Self {
        Self {
            max_rows: budget.effective_batch_rows(),
            max_bytes: budget.effective_max_batch_bytes(),
            rows: 0,
            bytes: 0,
        }
    }

    /// Records one decoded row of estimated size `row_bytes`.
    pub fn add_row(&mut self, row_bytes: u64) {
        self.rows = self.rows.saturating_add(1);
        self.bytes = self.bytes.saturating_add(row_bytes);
    }

    /// `true` when the batch must be emitted now (row or byte limit reached).
    #[must_use]
    pub fn is_full(&self) -> bool {
        self.rows >= self.max_rows || self.bytes >= self.max_bytes
    }

    /// Rows in the current batch.
    #[must_use]
    pub fn rows(&self) -> u32 {
        self.rows
    }

    /// Estimated bytes of the current batch.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Starts the next batch.
    pub fn reset(&mut self) {
        self.rows = 0;
        self.bytes = 0;
    }
}

/// Cuts `value` to at most `max_bytes` bytes on a `char` boundary. The flag is `true` when it
/// was cut.
#[must_use]
pub fn truncate_str(value: &str, max_bytes: u64) -> (&str, bool) {
    let max = usize::try_from(max_bytes).unwrap_or(usize::MAX);
    if value.len() <= max {
        return (value, false);
    }
    let mut end = max;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    (&value[..end], true)
}

/// Cuts `value` to at most `max_bytes` bytes. The flag is `true` when it was cut.
#[must_use]
pub fn truncate_bytes(value: &[u8], max_bytes: u64) -> (&[u8], bool) {
    let max = usize::try_from(max_bytes).unwrap_or(usize::MAX);
    if value.len() <= max {
        (value, false)
    } else {
        (&value[..max], true)
    }
}

/// Options for one [`Session::execute`](crate::Session::execute) call.
///
/// Non-exhaustive: build from `ExecOptions::default()` and the `with_*` methods.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct ExecOptions {
    /// Wall-clock limit for the whole execution, including fetching. `None` = no limit
    /// (interactive default, ADR-0004). On expiry the statement is cancelled (server-side when
    /// supported) and the stream yields [`DriverError::Timeout`](crate::DriverError::Timeout).
    pub timeout: Option<Duration>,
    /// Memory budget.
    pub budget: FetchBudget,
    /// Cancelled by the user ("Stop" button). The stream yields
    /// [`DriverError::Cancelled`](crate::DriverError::Cancelled).
    pub cancel: CancelToken,
}

impl ExecOptions {
    /// Sets the timeout.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.timeout = timeout;
        self
    }

    /// Sets the budget.
    #[must_use]
    pub fn with_budget(mut self, budget: FetchBudget) -> Self {
        self.budget = budget;
        self
    }

    /// Sets the cancel token.
    #[must_use]
    pub fn with_cancel(mut self, cancel: CancelToken) -> Self {
        self.cancel = cancel;
        self
    }
}

/// A cloneable cancellation flag shared between the UI side and the running stream.
#[derive(Clone, Default)]
pub struct CancelToken {
    inner: Arc<CancelInner>,
}

#[derive(Default)]
struct CancelInner {
    cancelled: AtomicBool,
    notify: Notify,
}

impl CancelToken {
    /// A fresh, not-cancelled token.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Requests cancellation. Idempotent.
    pub fn cancel(&self) {
        self.inner.cancelled.store(true, Ordering::SeqCst);
        self.inner.notify.notify_waiters();
    }

    /// `true` once [`cancel`](Self::cancel) was called.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::SeqCst)
    }

    /// Completes when the token is cancelled.
    pub async fn cancelled(&self) {
        loop {
            let notified = self.inner.notify.notified();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

impl fmt::Debug for CancelToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CancelToken")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_hides_text_and_values() {
        let s = Statement::sql("select * from t where secret = $1")
            .with_params(vec![Param::Text("top-secret-value".into()), Param::Int(42)]);
        let dbg = format!("{s:?}");
        assert!(!dbg.contains("top-secret-value"));
        assert!(!dbg.contains("42"));
        assert!(!dbg.contains("select"));
        assert!(dbg.contains("Param::Text"));
    }

    #[test]
    fn budget_defaults_match_adr() {
        let b = FetchBudget::default();
        assert_eq!(b.batch_rows, 200);
        assert_eq!(b.max_bytes, 256 * 1024 * 1024);
        assert_eq!(ExecOptions::default().timeout, None);
        assert_eq!(b.with_batch_rows(0).effective_batch_rows(), 1);
        assert_eq!(
            b.with_batch_rows(u32::MAX).effective_batch_rows(),
            FetchBudget::MAX_BATCH_ROWS
        );
        assert_eq!(b.with_max_bytes(1000).effective_max_batch_bytes(), 1000);
    }

    #[test]
    fn batch_limiter_and_value_truncation() {
        let b = FetchBudget::default()
            .with_batch_rows(3)
            .with_max_batch_bytes(100);
        let mut l = BatchLimiter::new(&b);
        l.add_row(10);
        l.add_row(10);
        assert!(!l.is_full());
        l.add_row(10);
        assert!(l.is_full());
        l.reset();
        l.add_row(150);
        assert!(l.is_full());
        assert_eq!(truncate_str("héllo", 2), ("h", true));
        assert_eq!(truncate_str("hello", 10), ("hello", false));
        assert_eq!(truncate_bytes(b"abcdef", 3), (&b"abc"[..], true));
    }

    #[tokio::test]
    async fn cancel_token_wakes_waiters() {
        let t = CancelToken::new();
        let t2 = t.clone();
        let h = tokio::spawn(async move { t2.cancelled().await });
        tokio::task::yield_now().await;
        t.cancel();
        h.await.unwrap();
        assert!(t.is_cancelled());
        // Already cancelled: returns immediately.
        t.cancelled().await;
    }
}
