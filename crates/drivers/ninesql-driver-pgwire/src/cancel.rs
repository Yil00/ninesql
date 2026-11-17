//! Server-side cancel: a `CancelRequest` with the backend key on a new connection to the same
//! address with the same TLS settings, plus the idle-cursor flag (docs/08): a portal suspended
//! between `Execute` calls is not "running", so the server ignores the request; the flag makes
//! the connection task close the portal instead of fetching more rows.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use bytes::BytesMut;
use ninesql_driver_api::{CancelHandle, DriverResult, SERVER_CANCEL_GRACE};
use postgres_protocol::message::frontend;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Notify;

use crate::conn::open_transport_addr;
use crate::tls::TlsSetup;

/// Where and how to send a cancel request. The backend key authenticates the request; no
/// password is needed (`retains_secret_after_connect = false`).
#[derive(Clone, Debug)]
pub(crate) struct CancelTarget {
    pub(crate) addr: SocketAddr,
    pub(crate) tls: Option<TlsSetup>,
    pub(crate) process_id: i32,
    pub(crate) secret_key: i32,
}

impl CancelTarget {
    /// Sends one cancel request and waits until the server closed the cancel connection (it has
    /// then signalled the backend), bounded by [`SERVER_CANCEL_GRACE`].
    pub(crate) async fn send(&self) -> DriverResult<()> {
        let run = async {
            let mut io = open_transport_addr(self.addr, self.tls.as_ref()).await?;
            let mut buf = BytesMut::new();
            frontend::cancel_request(self.process_id, self.secret_key, &mut buf);
            io.write_all(&buf).await.map_err(crate::conn::io_err)?;
            io.flush().await.map_err(crate::conn::io_err)?;
            let mut sink = [0u8; 64];
            // The server answers nothing and closes the connection.
            while io.read(&mut sink).await.map_err(crate::conn::io_err)? > 0 {}
            DriverResult::Ok(())
        };
        match tokio::time::timeout(SERVER_CANCEL_GRACE, run).await {
            Ok(r) => r,
            Err(_) => {
                tracing::warn!("cancel request not acknowledged in time");
                Ok(())
            }
        }
    }
}

/// Per-execution cancel state read by the connection task.
#[derive(Debug, Default)]
pub(crate) struct ExecCancel {
    requested: AtomicBool,
    notify: Notify,
    /// Cancel requests of [`PgCancel::cancel`] still on their way to the server.
    in_flight: AtomicUsize,
    sent: Notify,
    /// The connection task began to end the statement (security review M1): a cancel that
    /// comes later raises the flag but sends nothing to the server. With `in_flight`, a
    /// Dekker-style pair (both stored, then the other loaded, all `SeqCst`): either the task
    /// sees the request in flight and waits for it, or the request sees `ending` and is not
    /// sent, never neither.
    ending: AtomicBool,
    /// A cancel came after `ending` (security review M-1): the connection task, which then
    /// runs the end round trip of the driver's implicit transaction, cancels that round trip
    /// itself, once, and bounds it.
    late: AtomicBool,
    late_notify: Notify,
}

impl ExecCancel {
    pub(crate) fn request(&self) {
        self.requested.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    pub(crate) fn is_requested(&self) -> bool {
        self.requested.load(Ordering::SeqCst)
    }

    /// Called by the connection task before the end round trip of the statement (Close
    /// portal, `COMMIT` / `ROLLBACK`, the session checks). Closes the gate, then waits until no
    /// cancel request of [`PgCancel::cancel`] is still on its way to the server, bounded by
    /// [`SERVER_CANCEL_GRACE`]. A request that reached the server during the end round trip
    /// would interrupt it instead of the statement, leaving the statement's transaction
    /// aborted. (It cannot reach the next statement: the session's `ExecutionTracker` keeps
    /// the next one out until `cancel()` returned.) The wait is bounded: a request still on
    /// its way after [`SERVER_CANCEL_GRACE`] may still hit the end round trip, and then the
    /// post-end `ROLLBACK` check of the implicit transaction applies. A cancel that comes after
    /// the gate closed sends nothing itself; it raises [`late_requested`](Self::late_requested)
    /// for the connection task.
    pub(crate) async fn begin_end(&self) {
        self.ending.store(true, Ordering::SeqCst);
        self.requests_sent().await;
    }

    /// Waits until no cancel request of [`PgCancel::cancel`] is still on its way to the server,
    /// bounded by [`SERVER_CANCEL_GRACE`]; beyond it the post-end `ROLLBACK` check applies (see
    /// [`begin_end`](Self::begin_end)).
    pub(crate) async fn requests_sent(&self) {
        let wait = async {
            loop {
                let sent = self.sent.notified();
                if self.in_flight.load(Ordering::SeqCst) == 0 {
                    return;
                }
                sent.await;
            }
        };
        let _ = tokio::time::timeout(SERVER_CANCEL_GRACE, wait).await;
    }

    /// Completes once a cancel was requested.
    pub(crate) async fn requested(&self) {
        loop {
            let notified = self.notify.notified();
            if self.is_requested() {
                return;
            }
            notified.await;
        }
    }

    /// Completes once a cancel came after [`begin_end`](Self::begin_end) closed the gate.
    pub(crate) async fn late_requested(&self) {
        loop {
            let notified = self.late_notify.notified();
            if self.late.load(Ordering::SeqCst) {
                return;
            }
            notified.await;
        }
    }

    /// One user cancel (security review M1, L1): counted in flight, then the flag, then the
    /// server request through `send` unless the statement already began to end. The count is
    /// released by a drop guard, so a caller that drops the future mid-send (the driver API
    /// bounds `cancel()` by [`SERVER_CANCEL_GRACE`]) never leaves it raised.
    async fn cancel_with<F, Fut>(&self, send: F)
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = DriverResult<()>>,
    {
        // Counted before the gate is read (the Dekker pair with `begin_end`) and before the
        // flag, so the connection task, woken by the flag, waits for the request.
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        let _in_flight = InFlight(self);
        // Flag first: if the portal is idle, the connection task closes it at once.
        self.request();
        if self.ending.load(Ordering::SeqCst) {
            // The end round trip may itself block (a deferred trigger, a synchronous standby
            // that is down): the connection task, which knows what it runs, cancels it.
            tracing::debug!("cancel after the statement began to end: left to the connection task");
            self.late.store(true, Ordering::SeqCst);
            self.late_notify.notify_waiters();
            return;
        }
        if let Err(e) = send().await {
            tracing::warn!(error = %e, "cancel request failed");
        }
    }
}

/// Releases one in-flight cancel request, even when the cancel future is dropped.
struct InFlight<'a>(&'a ExecCancel);

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
        self.0.sent.notify_waiters();
    }
}

/// The session's unscoped engine cancel for one execution; the session scopes it with its
/// `ExecutionTracker`, so a late call never reaches the next statement.
pub(crate) struct PgCancel {
    pub(crate) target: Arc<CancelTarget>,
    pub(crate) exec: Arc<ExecCancel>,
}

#[async_trait::async_trait]
impl CancelHandle for PgCancel {
    async fn cancel(&self) -> DriverResult<()> {
        self.exec.cancel_with(|| self.target.send()).await;
        Ok(())
    }

    fn confirms_end(&self) -> bool {
        true
    }
}

/// Cancel gate of one navigator statement (security review M1). A server-side cancel is sent
/// only while the statement itself runs, never while the driver sets up or releases the
/// savepoint that protects the user's open transaction; a call abandoned before its statement
/// started does not run it.
#[derive(Debug, Default)]
pub(crate) struct MetaGate {
    running: AtomicBool,
    abandoned: AtomicBool,
}

impl MetaGate {
    /// The caller dropped the call: `true` when the statement runs now and may be cancelled.
    pub(crate) fn abandon(&self) -> bool {
        self.abandoned.store(true, Ordering::SeqCst);
        self.running.load(Ordering::SeqCst)
    }

    /// The statement is about to run: `false` when the caller already dropped the call.
    pub(crate) fn start(&self) -> bool {
        self.running.store(true, Ordering::SeqCst);
        !self.abandoned.load(Ordering::SeqCst)
    }

    /// The statement ended.
    pub(crate) fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;
    use std::time::Duration;
    use tokio::sync::oneshot;

    fn in_flight(exec: &ExecCancel) -> usize {
        exec.in_flight.load(Ordering::SeqCst)
    }

    /// Lets spawned tasks run until `cond` holds (paused time: no real wait).
    async fn until(cond: impl Fn() -> bool) {
        for _ in 0..1000 {
            if cond() {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("condition not reached");
    }

    #[tokio::test(start_paused = true)]
    async fn requests_sent_waits_for_an_in_flight_request() {
        let exec = Arc::new(ExecCancel::default());
        let (release, released) = oneshot::channel::<()>();
        let sends = Arc::new(AtomicU32::new(0));
        let cancel = tokio::spawn({
            let exec = Arc::clone(&exec);
            let sends = Arc::clone(&sends);
            async move {
                exec.cancel_with(|| async move {
                    let _ = released.await;
                    sends.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
                .await;
            }
        });
        until(|| in_flight(&exec) == 1).await;
        assert!(exec.is_requested());
        let waiter = tokio::spawn({
            let exec = Arc::clone(&exec);
            async move { exec.requests_sent().await }
        });
        // Still waiting while the request is on its way.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!waiter.is_finished());
        release.send(()).unwrap();
        waiter.await.unwrap();
        cancel.await.unwrap();
        assert_eq!(sends.load(Ordering::SeqCst), 1);
        assert_eq!(in_flight(&exec), 0);
        // Nothing in flight: immediate.
        let started = tokio::time::Instant::now();
        exec.requests_sent().await;
        assert_eq!(started.elapsed(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn requests_sent_is_bounded() {
        let exec = Arc::new(ExecCancel::default());
        let cancel = tokio::spawn({
            let exec = Arc::clone(&exec);
            async move {
                exec.cancel_with(std::future::pending::<DriverResult<()>>)
                    .await;
            }
        });
        until(|| in_flight(&exec) == 1).await;
        let started = tokio::time::Instant::now();
        exec.requests_sent().await;
        let waited = started.elapsed();
        assert!(
            waited >= SERVER_CANCEL_GRACE && waited < SERVER_CANCEL_GRACE * 2,
            "{waited:?}"
        );
        assert_eq!(in_flight(&exec), 1);
        cancel.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn a_dropped_cancel_releases_its_count() {
        let exec = Arc::new(ExecCancel::default());
        // The driver API's bound drops the future mid-send.
        let r = tokio::time::timeout(
            Duration::from_millis(10),
            exec.cancel_with(std::future::pending::<DriverResult<()>>),
        )
        .await;
        assert!(r.is_err());
        assert!(exec.is_requested());
        assert_eq!(in_flight(&exec), 0);
        let started = tokio::time::Instant::now();
        exec.requests_sent().await;
        assert_eq!(started.elapsed(), Duration::ZERO);

        // A waiter blocked on the count is woken by the drop.
        let exec = Arc::new(ExecCancel::default());
        let cancel = tokio::spawn({
            let exec = Arc::clone(&exec);
            async move {
                exec.cancel_with(std::future::pending::<DriverResult<()>>)
                    .await;
            }
        });
        until(|| in_flight(&exec) == 1).await;
        let waiter = tokio::spawn({
            let exec = Arc::clone(&exec);
            async move {
                let started = tokio::time::Instant::now();
                exec.requests_sent().await;
                started.elapsed()
            }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel.abort();
        let waited = waiter.await.unwrap();
        assert!(waited < SERVER_CANCEL_GRACE, "{waited:?}");
        assert_eq!(in_flight(&exec), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_cancel_after_the_end_began_sends_nothing() {
        let exec = ExecCancel::default();
        let started = tokio::time::Instant::now();
        exec.begin_end().await;
        assert_eq!(started.elapsed(), Duration::ZERO);
        let sends = AtomicU32::new(0);
        exec.cancel_with(|| async {
            sends.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .await;
        assert_eq!(sends.load(Ordering::SeqCst), 0);
        // The flag is still raised, the connection task is told; the count is released.
        assert!(exec.is_requested());
        tokio::time::timeout(Duration::from_millis(1), exec.late_requested())
            .await
            .unwrap();
        assert_eq!(in_flight(&exec), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_late_cancel_wakes_the_connection_task() {
        let exec = Arc::new(ExecCancel::default());
        exec.begin_end().await;
        let waiter = tokio::spawn({
            let exec = Arc::clone(&exec);
            async move { exec.late_requested().await }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!waiter.is_finished());
        exec.cancel_with(|| async { Ok(()) }).await;
        tokio::time::timeout(Duration::from_millis(1), waiter)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn the_end_waits_for_a_cancel_sent_before_it() {
        let exec = Arc::new(ExecCancel::default());
        let (release, released) = oneshot::channel::<()>();
        let sends = Arc::new(AtomicU32::new(0));
        let cancel = tokio::spawn({
            let exec = Arc::clone(&exec);
            let sends = Arc::clone(&sends);
            async move {
                exec.cancel_with(|| async move {
                    let _ = released.await;
                    sends.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
                .await;
            }
        });
        until(|| in_flight(&exec) == 1).await;
        let end = tokio::spawn({
            let exec = Arc::clone(&exec);
            async move { exec.begin_end().await }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!end.is_finished(), "the end did not wait for the request");
        release.send(()).unwrap();
        end.await.unwrap();
        cancel.await.unwrap();
        // The request that was already on its way was sent; it is not a late cancel.
        assert_eq!(sends.load(Ordering::SeqCst), 1);
        assert!(!exec.late.load(Ordering::SeqCst));
    }

    /// Both orders of the gate, under real thread interleavings: a request is either sent and
    /// awaited by the end, or not sent; never sent after the end stopped waiting.
    #[test]
    fn the_ending_gate_holds_across_threads() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_time()
            .build()
            .unwrap();
        rt.block_on(async {
            for _ in 0..2000 {
                let exec = Arc::new(ExecCancel::default());
                let ended = Arc::new(AtomicBool::new(false));
                let late = Arc::new(AtomicBool::new(false));
                let cancel = tokio::spawn({
                    let exec = Arc::clone(&exec);
                    let ended = Arc::clone(&ended);
                    let late = Arc::clone(&late);
                    async move {
                        exec.cancel_with(|| async move {
                            tokio::task::yield_now().await;
                            if ended.load(Ordering::SeqCst) {
                                late.store(true, Ordering::SeqCst);
                            }
                            Ok(())
                        })
                        .await;
                    }
                });
                exec.begin_end().await;
                ended.store(true, Ordering::SeqCst);
                cancel.await.unwrap();
                assert!(
                    !late.load(Ordering::SeqCst),
                    "a request was sent after the end"
                );
            }
        });
    }
}
