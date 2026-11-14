//! The driver runtime (ADR-0004): one multi-thread Tokio runtime owned by the core, dedicated to
//! driver and network I/O (sessions, SSH tunnels, secret store calls on its blocking pool).
//!
//! The UI never runs on it and never awaits it: services spawn work here and hand results back
//! through channels ([`crate::session::Reply`], [`crate::session::Execution`]) whose receivers
//! work on any executor, so GPUI polls them on its own (I6).

use std::fmt;
use std::future::Future;
use std::time::Duration;

use tokio::runtime::{Builder, Handle, Runtime};
use tokio::task::JoinHandle;

use crate::error::CoreError;

/// Default worker count (ADR-0004: bounded, default 4).
pub const DEFAULT_WORKERS: usize = 4;

/// Largest worker count accepted.
pub const MAX_WORKERS: usize = 16;

/// Largest number of threads of the blocking pool (secret store calls, `spawn_blocking` of
/// synchronous drivers, `known_hosts` file I/O).
const MAX_BLOCKING_THREADS: usize = 64;

/// The core's Tokio runtime. Dropping it stops it in the background without blocking, so it may
/// be dropped from any thread, including a Tokio worker.
pub struct DriverRuntime {
    runtime: Option<Runtime>,
    handle: Handle,
}

impl fmt::Debug for DriverRuntime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DriverRuntime")
            .field("workers", &self.handle.metrics().num_workers())
            .finish()
    }
}

impl DriverRuntime {
    /// Starts the runtime with `workers` worker threads, clamped to `1..=MAX_WORKERS`. Threads
    /// are named `ninesql-driver`.
    ///
    /// # Errors
    /// [`CoreError::Runtime`] when the threads cannot be created.
    pub fn new(workers: usize) -> Result<Self, CoreError> {
        let runtime = Builder::new_multi_thread()
            .worker_threads(workers.clamp(1, MAX_WORKERS))
            .max_blocking_threads(MAX_BLOCKING_THREADS)
            .thread_name("ninesql-driver")
            .enable_io()
            .enable_time()
            .build()
            .map_err(|e| CoreError::Runtime(e.to_string()))?;
        let handle = runtime.handle().clone();
        tracing::debug!(
            workers = handle.metrics().num_workers(),
            "driver runtime started"
        );
        Ok(Self {
            runtime: Some(runtime),
            handle,
        })
    }

    /// Handle to spawn on the runtime from any thread.
    #[must_use]
    pub fn handle(&self) -> &Handle {
        &self.handle
    }

    /// Spawns a task on the runtime. Never blocks.
    pub fn spawn<F>(&self, future: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.handle.spawn(future)
    }

    /// Stops the runtime, waiting at most `timeout` for running tasks to notice. Must not be
    /// called from inside an async context (use drop there).
    pub fn shutdown_timeout(mut self, timeout: Duration) {
        if let Some(rt) = self.runtime.take() {
            rt.shutdown_timeout(timeout);
        }
    }
}

impl Drop for DriverRuntime {
    fn drop(&mut self) {
        if let Some(rt) = self.runtime.take() {
            rt.shutdown_background();
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).
mod tests {
    use super::*;

    #[test]
    fn spawns_on_named_workers_and_clamps() {
        let rt = DriverRuntime::new(0).unwrap();
        assert_eq!(rt.handle().metrics().num_workers(), 1);
        let (tx, rx) = std::sync::mpsc::channel();
        rt.spawn(async move {
            let name = std::thread::current().name().map(str::to_owned);
            tx.send(name).unwrap();
        });
        let name = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(name.as_deref(), Some("ninesql-driver"));
        rt.shutdown_timeout(Duration::from_secs(1));
        assert_eq!(
            DriverRuntime::new(1000)
                .unwrap()
                .handle()
                .metrics()
                .num_workers(),
            MAX_WORKERS
        );
    }

    #[test]
    fn dropping_inside_the_runtime_does_not_panic() {
        let rt = std::sync::Arc::new(DriverRuntime::new(1).unwrap());
        let inner = std::sync::Arc::clone(&rt);
        let (tx, rx) = std::sync::mpsc::channel();
        rt.spawn(async move {
            // The last reference may be dropped on a worker.
            drop(inner);
            tx.send(()).unwrap();
        });
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        drop(rt);
    }
}
