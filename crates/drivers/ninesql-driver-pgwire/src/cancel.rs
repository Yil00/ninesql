//! Server-side cancel: a `CancelRequest` with the backend key on a new connection to the same
//! address with the same TLS settings, plus the idle-cursor flag (docs/08): a portal suspended
//! between `Execute` calls is not "running", so the server ignores the request; the flag makes
//! the connection task close the portal instead of fetching more rows.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

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
pub(crate) struct PgCancel {
    pub(crate) target: Arc<CancelTarget>,
    pub(crate) exec: Arc<ExecCancel>,
}

#[async_trait::async_trait]
impl CancelHandle for PgCancel {
    async fn cancel(&self) -> DriverResult<()> {
        // Flag first: if the portal is idle, the connection task closes it at once.
        self.exec.request();
        if let Err(e) = self.target.send().await {
            tracing::warn!(error = %e, "cancel request failed");
        }
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
