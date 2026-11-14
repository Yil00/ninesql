//! Execution-scoped bookkeeping: execution ids, the session's busy state and cancel handles
//! scoped to one execution (M6 of the P0-B security review).
//!
//! # The race this prevents
//! Most engines cancel "whatever the connection is running" (PostgreSQL cancel request, MySQL
//! `KILL QUERY <thread>`, SQLite interrupt). A cancel that arrives late (the user clicked
//! "Stop" just as the statement finished, a timeout fired during the last batch) could then
//! hit the **next** statement of the session. Two rules close it:
//! 1. **Scoped handles**: every cancel handle a driver hands out is bound to one
//!    [`ExecutionId`]. Once that execution ended, the handle does nothing.
//! 2. **Busy until confirmed**: the session stays busy ([`DriverError::Busy`]) until the server
//!    confirmed that the statement ended (normal completion, error, or acknowledged cancel) and
//!    until every cancel request sent for it has returned, so no new statement can start while
//!    a cancel for the previous one is still in flight.
//!
//! [`ExecutionTracker`] implements both for drivers: `execute` calls
//! [`ExecutionTracker::begin`], keeps the [`ExecutionGuard`] inside its raw stream, completes it
//! when the server confirms the end, and builds the stream's cancel handle with
//! [`ExecutionTracker::cancel_handle`]. Engines whose cancel is addressed to one statement
//! (Trino query id, ClickHouse `query_id`, Elasticsearch task id, MongoDB `opId`) still use the
//! tracker for the busy state, and send the execution's own id to the server.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tokio::sync::Notify;

use crate::error::{DriverError, DriverResult};
use crate::result::CancelHandle;

/// Identifier of one execution (one [`Session::execute`](crate::Session::execute) call) within a
/// session. Ids increase monotonically per [`ExecutionTracker`] and are never reused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ExecutionId(u64);

impl ExecutionId {
    /// The numeric value (for logs and engine-side labels such as a ClickHouse `query_id`).
    #[must_use]
    pub fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for ExecutionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "exec-{}", self.0)
    }
}

#[derive(Debug, Default)]
struct TrackerState {
    /// The execution that owns the session, from `begin` until its end is confirmed.
    current: Option<ExecutionId>,
    /// The guard of `current` was dropped without confirmation: waiting for
    /// [`ExecutionTracker::confirm_ended`].
    ending: bool,
    /// Cancel requests sent and not yet returned.
    cancels_in_flight: u32,
}

#[derive(Debug, Default)]
struct TrackerInner {
    next_id: AtomicU64,
    state: Mutex<TrackerState>,
    idle: Notify,
}

impl TrackerInner {
    fn lock(&self) -> MutexGuard<'_, TrackerState> {
        // The state stays consistent under poisoning: every critical section is a few
        // assignments without panics.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Per-session execution bookkeeping, shared (cheap clones) between the session, its raw
/// streams and its cancel handles. See the [module documentation](self).
#[derive(Clone, Debug, Default)]
pub struct ExecutionTracker {
    inner: Arc<TrackerInner>,
}

impl ExecutionTracker {
    /// A tracker with no execution running.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Starts an execution.
    ///
    /// # Errors
    /// [`DriverError::Busy`] while a previous execution has not been confirmed ended or a cancel
    /// request for it is still in flight.
    pub fn begin(&self) -> DriverResult<ExecutionGuard> {
        let mut st = self.inner.lock();
        if st.current.is_some() || st.cancels_in_flight > 0 {
            return Err(DriverError::Busy);
        }
        let id = ExecutionId(self.inner.next_id.fetch_add(1, Ordering::SeqCst) + 1);
        st.current = Some(id);
        st.ending = false;
        drop(st);
        Ok(ExecutionGuard {
            tracker: self.clone(),
            id,
            done: false,
        })
    }

    /// `true` while an execution owns the session or a cancel request is in flight.
    #[must_use]
    pub fn is_busy(&self) -> bool {
        let st = self.inner.lock();
        st.current.is_some() || st.cancels_in_flight > 0
    }

    /// The execution owning the session, if any.
    #[must_use]
    pub fn current(&self) -> Option<ExecutionId> {
        self.inner.lock().current
    }

    /// `true` when the guard of the current execution was dropped before the server confirmed
    /// its end (the driver must call [`confirm_ended`](Self::confirm_ended) once it has).
    #[must_use]
    pub fn is_ending(&self) -> bool {
        let st = self.inner.lock();
        st.current.is_some() && st.ending
    }

    /// Records that the server confirmed the end of execution `id` (completion, error, or
    /// acknowledged cancel), freeing the session. Ignored when `id` is not the current
    /// execution.
    pub fn confirm_ended(&self, id: ExecutionId) {
        let mut st = self.inner.lock();
        if st.current == Some(id) {
            st.current = None;
            st.ending = false;
        }
        let idle = st.current.is_none() && st.cancels_in_flight == 0;
        drop(st);
        if idle {
            self.inner.idle.notify_waiters();
        }
    }

    /// Completes when the session is idle (no execution, no cancel in flight). Callers bound it
    /// with their own timeout.
    pub async fn wait_idle(&self) {
        loop {
            let notified = self.inner.idle.notified();
            if !self.is_busy() {
                return;
            }
            notified.await;
        }
    }

    /// A cancel handle scoped to execution `id`: it forwards to `engine` (the driver's unscoped
    /// server-side cancel) only while `id` is the current execution, and keeps the session busy
    /// until the request has returned. Pass it to [`ResultStream::new`](crate::ResultStream::new).
    #[must_use]
    pub fn cancel_handle(
        &self,
        id: ExecutionId,
        engine: Arc<dyn CancelHandle>,
    ) -> Arc<dyn CancelHandle> {
        Arc::new(ScopedCancel {
            tracker: self.clone(),
            id: Some(id),
            engine,
        })
    }

    /// A cancel handle scoped to the execution running **now**, or a handle that does nothing
    /// when the session is idle. This is what
    /// [`Session::cancel_handle`](crate::Session::cancel_handle) returns.
    #[must_use]
    pub fn current_cancel_handle(&self, engine: Arc<dyn CancelHandle>) -> Arc<dyn CancelHandle> {
        Arc::new(ScopedCancel {
            tracker: self.clone(),
            id: self.current(),
            engine,
        })
    }
}

/// Ownership of the session by one execution. Drivers keep it inside the raw stream.
///
/// - [`complete`](Self::complete) when the server confirmed the end of the statement (the last
///   message of the result, an error, or the acknowledgement of a cancel): the session is free.
/// - Dropping it without `complete` (the stream was dropped mid-result, or the server did not
///   confirm in time) leaves the session **busy** until the driver calls
///   [`ExecutionTracker::confirm_ended`], typically from the task that drains the connection,
///   or until the driver closes the session.
#[must_use = "dropping the guard without `complete` keeps the session busy"]
pub struct ExecutionGuard {
    tracker: ExecutionTracker,
    id: ExecutionId,
    done: bool,
}

impl ExecutionGuard {
    /// Id of this execution.
    #[must_use]
    pub fn id(&self) -> ExecutionId {
        self.id
    }

    /// The tracker this execution belongs to.
    #[must_use]
    pub fn tracker(&self) -> &ExecutionTracker {
        &self.tracker
    }

    /// The server confirmed that the statement ended: frees the session.
    pub fn complete(mut self) {
        self.done = true;
        self.tracker.confirm_ended(self.id);
    }
}

impl Drop for ExecutionGuard {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        let mut st = self.tracker.inner.lock();
        if st.current == Some(self.id) {
            st.ending = true;
            tracing::debug!(
                execution = self.id.get(),
                "execution dropped before the server confirmed its end; session stays busy"
            );
        }
    }
}

impl fmt::Debug for ExecutionGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExecutionGuard")
            .field("id", &self.id)
            .field("done", &self.done)
            .finish()
    }
}

struct ScopedCancel {
    tracker: ExecutionTracker,
    id: Option<ExecutionId>,
    engine: Arc<dyn CancelHandle>,
}

/// Decrements the in-flight counter even when the cancel future is dropped (timeout).
struct InFlight<'a>(&'a ExecutionTracker);

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        let mut st = self.0.inner.lock();
        st.cancels_in_flight = st.cancels_in_flight.saturating_sub(1);
        let idle = st.current.is_none() && st.cancels_in_flight == 0;
        drop(st);
        if idle {
            self.0.inner.idle.notify_waiters();
        }
    }
}

#[async_trait::async_trait]
impl CancelHandle for ScopedCancel {
    async fn cancel(&self) -> DriverResult<()> {
        let Some(id) = self.id else {
            return Ok(());
        };
        {
            let mut st = self.tracker.inner.lock();
            if st.current != Some(id) {
                tracing::debug!(
                    execution = id.get(),
                    "stale cancel ignored: the execution already ended"
                );
                return Ok(());
            }
            st.cancels_in_flight += 1;
        }
        let _in_flight = InFlight(&self.tracker);
        self.engine.cancel().await
    }

    fn confirms_end(&self) -> bool {
        self.engine.confirms_end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    #[derive(Default)]
    struct CountingCancel(AtomicU32);

    #[async_trait::async_trait]
    impl CancelHandle for CountingCancel {
        async fn cancel(&self) -> DriverResult<()> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test]
    async fn busy_until_confirmed_and_stale_cancels_ignored() {
        let t = ExecutionTracker::new();
        let engine = Arc::new(CountingCancel::default());
        let a = t.begin().unwrap();
        assert_eq!(t.begin().err(), Some(DriverError::Busy));
        let stale = t.cancel_handle(a.id(), engine.clone());
        let current = t.current_cancel_handle(engine.clone());
        let a_id = a.id();
        // Dropped without confirmation: still busy.
        drop(a);
        assert!(t.is_busy() && t.is_ending());
        assert_eq!(t.begin().err(), Some(DriverError::Busy));
        t.confirm_ended(a_id);
        assert!(!t.is_busy());

        let b = t.begin().unwrap();
        assert!(b.id() > a_id);
        stale.cancel().await.unwrap();
        current.cancel().await.unwrap();
        assert_eq!(engine.0.load(Ordering::SeqCst), 0, "stale cancel reached B");
        t.cancel_handle(b.id(), engine.clone())
            .cancel()
            .await
            .unwrap();
        assert_eq!(engine.0.load(Ordering::SeqCst), 1);
        b.complete();
        assert!(!t.is_busy());
        t.wait_idle().await;
        // Idle: the "current" handle does nothing.
        t.current_cancel_handle(engine.clone())
            .cancel()
            .await
            .unwrap();
        assert_eq!(engine.0.load(Ordering::SeqCst), 1);
    }

    struct PendingCancel;

    #[async_trait::async_trait]
    impl CancelHandle for PendingCancel {
        async fn cancel(&self) -> DriverResult<()> {
            std::future::pending().await
        }
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_in_flight_blocks_the_next_execution() {
        let t = ExecutionTracker::new();
        let a = t.begin().unwrap();
        let h = t.cancel_handle(a.id(), Arc::new(PendingCancel));
        let task = tokio::spawn(async move {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(1), h.cancel()).await;
        });
        tokio::task::yield_now().await;
        a.complete();
        // The statement ended, but the cancel request has not returned yet.
        assert_eq!(t.begin().err(), Some(DriverError::Busy));
        task.await.unwrap();
        // The timed-out request no longer counts.
        assert!(t.begin().is_ok());
    }
}
