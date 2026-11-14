//! Server-side cancel: `KILL QUERY <id>` on a **control connection** opened and authenticated
//! during `connect`, to the session's resolved address with the same TLS settings (L5), so the
//! password is dropped before `connect` returns (`retains_secret_after_connect = false`).
//!
//! The control connection only ever runs `KILL QUERY` and `COM_PING`. A keep-alive pings it
//! every [`KEEPALIVE`] so the server's `wait_timeout` does not close it while the session is
//! idle; if it is lost anyway, cancel falls back to the drop path (the session's statement is
//! drained, bounded by `SERVER_CANCEL_GRACE`, and the session is closed when the server does not
//! stop in time).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use ninesql_driver_api::{CancelHandle, DriverError, DriverResult, SERVER_CANCEL_GRACE};
use tokio::sync::{Mutex, Notify};

use crate::conn::Conn;

/// Interval of the control connection's keep-alive ping.
pub(crate) const KEEPALIVE: Duration = Duration::from_secs(120);

/// The control connection and the id of the session's connection it cancels.
pub(crate) struct Control {
    conn: Mutex<Option<Conn>>,
    /// `CONNECTION_ID()` of the session's connection.
    pub(crate) target: AtomicU64,
}

impl std::fmt::Debug for Control {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Control")
            .field("target", &self.target)
            .finish_non_exhaustive()
    }
}

impl Control {
    pub(crate) fn new(conn: Conn) -> Arc<Self> {
        let control = Arc::new(Self {
            conn: Mutex::new(Some(conn)),
            target: AtomicU64::new(0),
        });
        let weak = Arc::downgrade(&control);
        tokio::spawn(keepalive(weak));
        control
    }

    /// Runs a statement on the control connection (setup only: read-only mode), bounded by
    /// [`SERVER_CANCEL_GRACE`]. A lost control connection has nothing to configure (`Ok`); a
    /// control connection that does not answer in time is dropped and the call fails with
    /// [`DriverError::Timeout`].
    pub(crate) async fn setup(&self, sql: &str) -> DriverResult<()> {
        let run = async {
            let mut g = self.conn.lock().await;
            let Some(c) = g.as_mut() else {
                tracing::debug!("control connection lost: nothing to configure");
                return Ok(());
            };
            match c.query(sql).await {
                Ok(Ok(_)) => Ok(()),
                Ok(Err(e)) => Err(DriverError::Connect(format!(
                    "control connection setup failed: {}",
                    e.display()
                ))),
                Err(e) => {
                    *g = None;
                    Err(e)
                }
            }
        };
        match tokio::time::timeout(SERVER_CANCEL_GRACE, run).await {
            Ok(r) => r,
            Err(_) => {
                // The connection is in an unknown state: drop it (the timed-out future released
                // the lock).
                if let Ok(mut g) = self.conn.try_lock()
                    && let Some(mut c) = g.take()
                {
                    drop(tokio::spawn(async move { c.quit().await }));
                }
                Err(DriverError::Timeout(SERVER_CANCEL_GRACE))
            }
        }
    }

    /// Sends `KILL QUERY` for the session's connection, bounded by [`SERVER_CANCEL_GRACE`].
    pub(crate) async fn kill(&self) -> DriverResult<()> {
        let id = self.target.load(Ordering::SeqCst);
        let run = async {
            let mut g = self.conn.lock().await;
            let Some(c) = g.as_mut() else {
                return Err(DriverError::Closed("control connection lost".into()));
            };
            match c.query(&format!("KILL QUERY {id}")).await {
                Ok(Ok(_)) => Ok(()),
                Ok(Err(e)) => {
                    // Unknown thread (it ended) or no privilege: nothing more to do.
                    tracing::debug!(code = e.code, "KILL QUERY refused");
                    Ok(())
                }
                Err(e) => {
                    *g = None;
                    Err(e)
                }
            }
        };
        match tokio::time::timeout(SERVER_CANCEL_GRACE, run).await {
            Ok(r) => r,
            Err(_) => {
                tracing::warn!("KILL QUERY not acknowledged in time");
                // The connection is in an unknown state: drop it.
                if let Ok(mut g) = self.conn.try_lock() {
                    *g = None;
                }
                Ok(())
            }
        }
    }

    /// Closes the control connection.
    pub(crate) async fn close(&self) {
        if let Some(mut c) = self.conn.lock().await.take() {
            c.quit().await;
        }
    }
}

async fn keepalive(control: Weak<Control>) {
    loop {
        tokio::time::sleep(KEEPALIVE).await;
        let Some(c) = control.upgrade() else {
            return;
        };
        let Ok(mut g) = c.conn.try_lock() else {
            continue;
        };
        let Some(conn) = g.as_mut() else {
            return;
        };
        let ok = matches!(
            tokio::time::timeout(SERVER_CANCEL_GRACE, conn.ping()).await,
            Ok(Ok(Ok(())))
        );
        if !ok {
            tracing::warn!("control connection lost: cancel falls back to stop fetching");
            *g = None;
            return;
        }
    }
}

/// Per-execution cancel state read by the connection task.
#[derive(Debug, Default)]
pub(crate) struct ExecCancel {
    requested: AtomicBool,
    notify: Notify,
}

impl ExecCancel {
    pub(crate) fn request(&self) {
        self.requested.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    pub(crate) fn is_requested(&self) -> bool {
        self.requested.load(Ordering::SeqCst)
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
}

/// The session's unscoped engine cancel for one execution; the session scopes it with its
/// `ExecutionTracker`, so a late call never reaches the next statement.
pub(crate) struct MyCancel {
    pub(crate) control: Arc<Control>,
    pub(crate) exec: Arc<ExecCancel>,
}

#[async_trait::async_trait]
impl CancelHandle for MyCancel {
    async fn cancel(&self) -> DriverResult<()> {
        self.exec.request();
        if let Err(e) = self.control.kill().await {
            tracing::warn!(error = ?std::mem::discriminant(&e), "KILL QUERY failed");
        }
        Ok(())
    }

    fn confirms_end(&self) -> bool {
        true
    }
}
