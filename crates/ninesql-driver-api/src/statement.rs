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
#[derive(Clone, PartialEq)]
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
/// The stream never delivers more than `max_bytes` (estimated from Arrow buffer sizes and
/// document sizes) nor more than `max_rows` rows; it ends with
/// [`ResultItem::Truncated`](crate::ResultItem::Truncated) instead. Drivers size their batches
/// with `batch_rows`, which also bounds how far the server cursor runs ahead of the consumer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FetchBudget {
    /// Rows per batch fetched from the server (DBeaver's "fetch size").
    pub batch_rows: u32,
    /// Maximum bytes held by this result.
    pub max_bytes: u64,
    /// Maximum rows / documents delivered, if any.
    pub max_rows: Option<u64>,
}

impl FetchBudget {
    /// DBeaver's default fetch size.
    pub const DEFAULT_BATCH_ROWS: u32 = 200;
    /// Default per-result memory budget: 256 MiB.
    pub const DEFAULT_MAX_BYTES: u64 = 256 * 1024 * 1024;
}

impl Default for FetchBudget {
    fn default() -> Self {
        Self {
            batch_rows: Self::DEFAULT_BATCH_ROWS,
            max_bytes: Self::DEFAULT_MAX_BYTES,
            max_rows: None,
        }
    }
}

/// Options for one [`Session::execute`](crate::Session::execute) call.
#[derive(Clone, Debug, Default)]
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
