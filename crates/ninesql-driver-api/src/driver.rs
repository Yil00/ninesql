//! `Driver` and `Session` traits (ADR-0004).
//!
//! # Runtime
//! All async methods run on the core's Tokio runtime, never on the GPUI foreground executor.
//! Implementations must not block worker threads: synchronous clients (`rusqlite`, `duckdb`) go
//! through `tokio::task::spawn_blocking`.
//!
//! # Logging
//! Implementations log through `tracing` only, never a secret, a parameter value or a result
//! value. Query text is logged only at `trace` level and never when
//! [`ConnectParams::production`] is set.

use std::sync::Arc;

use crate::capabilities::{Capabilities, Feature, IsolationLevel};
use crate::dialect::Dialect;
use crate::error::{DriverError, DriverResult};
use crate::metadata::Metadata;
use crate::params::{ConnectParams, EngineId};
use crate::result::{CancelHandle, ResultStream};
use crate::statement::{ExecOptions, Statement};

/// One driver family (pgwire, mysqlwire, sqlite…), serving one or more engines (ADR-0003).
#[async_trait::async_trait]
pub trait Driver: Send + Sync {
    /// Family name, e.g. `"pgwire"`.
    fn family(&self) -> &'static str;

    /// Engines served by this family.
    fn engines(&self) -> &[EngineId];

    /// Capabilities of `engine`, optionally refined for a server version. Before connect the
    /// core calls it with `None` to build the connection dialog. A connected session reports
    /// its exact capabilities through [`Session::capabilities`].
    ///
    /// # Errors
    /// [`DriverError::InvalidParams`] for an engine this driver does not serve.
    fn capabilities(
        &self,
        engine: &EngineId,
        server_version: Option<&str>,
    ) -> DriverResult<Capabilities>;

    /// Dialect of `engine`, usable without a session (editor highlighting, script splitting).
    ///
    /// # Errors
    /// [`DriverError::InvalidParams`] for an engine this driver does not serve.
    fn dialect(&self, engine: &EngineId) -> DriverResult<Arc<dyn Dialect>>;

    /// Opens a session.
    ///
    /// Contract:
    /// - call [`check_properties`](crate::check_properties) first: driver properties never
    ///   override TLS / verification, host / port, credentials, read-only / access mode,
    ///   multi-statements, init SQL or lexical modes (reuse
    ///   `conformance::assert_reserved_properties_rejected` in the integration tests);
    /// - disable multi-statement support in the client (MySQL `CLIENT_MULTI_STATEMENTS` off);
    /// - honour [`ConnectParams::connect_timeout`] for the whole connect;
    /// - verify TLS according to [`TlsConfig`](crate::TlsConfig) with rustls; never fall back to
    ///   a weaker mode than requested;
    /// - when [`ConnectParams::read_only`] is set, apply the engine-side mechanism declared in
    ///   [`Capabilities::read_only`] **before** returning, verify it took effect where the engine
    ///   allows reading it back, and fail with [`DriverError::ReadOnly`] otherwise (I3);
    /// - run [`ConnectParams::init_statements`] after the read-only mechanism, so they cannot
    ///   lift it (they are subject to the same read-only refusal as user statements);
    /// - drop every [`Secret`](crate::Secret) as soon as the handshake is done, unless the
    ///   engine needs it for reconnects (documented per driver);
    /// - error messages are passed through
    ///   [`redact_connection_string`](crate::redact_connection_string) /
    ///   [`redact_secret`](crate::redact_secret) when the client may echo credentials.
    async fn connect(&self, params: ConnectParams) -> DriverResult<Box<dyn Session>>;
}

/// A live connection with its transaction state. Owned by the core, used from one task at a
/// time; [`Session::cancel_handle`] is the only part meant to be used concurrently.
#[async_trait::async_trait]
pub trait Session: Send + Sync {
    /// Exact capabilities for this engine and server version.
    fn capabilities(&self) -> &Capabilities;

    /// Server version string as reported by the engine.
    fn server_version(&self) -> Option<&str>;

    /// Dialect of this session's engine.
    fn dialect(&self) -> Arc<dyn Dialect>;

    /// Metadata access. Drivers may serve it from a separate connection so the navigator keeps
    /// working while a query streams.
    fn metadata(&self) -> Arc<dyn Metadata>;

    /// Executes one statement.
    ///
    /// Contract:
    /// - returns quickly with a [`ResultStream`] built by [`ResultStream::new`] around a lazy raw
    ///   stream, so the timeout, the cancel token and the budget in `options` cover the work;
    /// - sends exactly **one statement per protocol call** (PostgreSQL extended protocol, MySQL
    ///   without `CLIENT_MULTI_STATEMENTS`) and refuses text that
    ///   [`Dialect::split_statements`] splits into more than one span with
    ///   [`DriverError::MultipleStatements`] ([`ensure_single_statement`](crate::ensure_single_statement)),
    ///   so the statement the engine runs is the one that was classified;
    /// - builds the [`Dialect`]'s [`LexicalRules`](crate::LexicalRules) from the session's
    ///   current lexical modes (MySQL `NO_BACKSLASH_ESCAPES`, PostgreSQL
    ///   `standard_conforming_strings`) before classifying;
    /// - ends each batch at
    ///   [`FetchBudget::effective_batch_rows`](crate::FetchBudget::effective_batch_rows) rows or
    ///   [`FetchBudget::effective_max_batch_bytes`](crate::FetchBudget::effective_max_batch_bytes)
    ///   bytes, whichever comes first ([`BatchLimiter`](crate::BatchLimiter)), and cuts values
    ///   above [`FetchBudget::max_value_bytes`](crate::FetchBudget::max_value_bytes), flagging
    ///   the field with [`TRUNCATED_VALUES_METADATA_KEY`](crate::TRUNCATED_VALUES_METADATA_KEY);
    ///   never reads the whole result, or a whole oversized value, into memory (I6);
    /// - in a read-only session, refuses statements whose
    ///   [`Dialect::classify`] is a write with
    ///   [`DriverError::ReadOnly`] before sending them, in addition to the engine-side mechanism
    ///   that rejects anything the classifier misses (I3). Statements classified
    ///   [`ChangesAccessMode`](crate::StatementClass::ChangesAccessMode) are always refused in a
    ///   read-only session, since many engines let a client lift the session setting;
    /// - when the mechanism is [liftable](crate::ReadOnlyMechanism::is_liftable), verifies the
    ///   engine's access mode after every statement (PostgreSQL `ParameterStatus` for
    ///   `default_transaction_read_only`, MySQL session-state tracking of
    ///   `transaction_read_only`, or a read-back) and, if it changed, closes the session and
    ///   fails with [`DriverError::ReadOnly`]; later calls fail with [`DriverError::Closed`];
    /// - fails with [`DriverError::Busy`] while a previous [`ResultStream`] of this session is
    ///   still alive, unless the engine multiplexes;
    /// - fails with [`DriverError::Unsupported`] for a language or parameters not declared.
    async fn execute(
        &mut self,
        statement: Statement,
        options: ExecOptions,
    ) -> DriverResult<ResultStream>;

    /// Handle that stops the running statement on the server, usable from another task while a
    /// stream is consumed. [`ResultStream`] calls it on timeout and cancel.
    fn cancel_handle(&self) -> Arc<dyn CancelHandle>;

    /// Stops the running statement (shortcut for `cancel_handle().cancel()`).
    async fn cancel(&self) -> DriverResult<()> {
        self.cancel_handle().cancel().await
    }

    /// Current auto-commit mode.
    fn auto_commit(&self) -> bool;

    /// `true` while a transaction is open with uncommitted work (pending-transaction indicator,
    /// ADR-0007 §5).
    fn in_transaction(&self) -> bool;

    /// Switches auto-commit. Turning it off requires
    /// [`TransactionCapabilities::manual_commit`](crate::TransactionCapabilities::manual_commit).
    async fn set_auto_commit(&mut self, auto_commit: bool) -> DriverResult<()>;

    /// Starts a transaction explicitly.
    async fn begin(&mut self) -> DriverResult<()> {
        Err(DriverError::Unsupported(Feature::Transactions))
    }

    /// Commits the current transaction.
    async fn commit(&mut self) -> DriverResult<()> {
        Err(DriverError::Unsupported(Feature::Transactions))
    }

    /// Rolls back the current transaction.
    async fn rollback(&mut self) -> DriverResult<()> {
        Err(DriverError::Unsupported(Feature::Transactions))
    }

    /// Creates a savepoint.
    async fn savepoint(&mut self, name: &str) -> DriverResult<()> {
        let _ = name;
        Err(DriverError::Unsupported(Feature::Savepoints))
    }

    /// Rolls back to a savepoint.
    async fn rollback_to_savepoint(&mut self, name: &str) -> DriverResult<()> {
        let _ = name;
        Err(DriverError::Unsupported(Feature::Savepoints))
    }

    /// Sets the isolation level of the next transactions.
    async fn set_isolation_level(&mut self, level: IsolationLevel) -> DriverResult<()> {
        let _ = level;
        Err(DriverError::Unsupported(Feature::Transactions))
    }

    /// `true` when the session is read-only.
    fn is_read_only(&self) -> bool;

    /// Switches read-only mode with the engine-side mechanism (I3). Switching on fails with
    /// [`DriverError::ReadOnly`] if the mechanism cannot be applied; the session then stays in
    /// its previous mode and the core must not treat it as read-only. Engines whose mechanism is
    /// fixed at open time ([`ReadOnlyMechanism::OpenMode`](crate::ReadOnlyMechanism::OpenMode))
    /// may return [`DriverError::Unsupported`] and require a reconnect.
    async fn set_read_only(&mut self, read_only: bool) -> DriverResult<()>;

    /// Cheap liveness check.
    async fn ping(&mut self) -> DriverResult<()>;

    /// Closes the session, rolling back any open transaction.
    async fn close(self: Box<Self>) -> DriverResult<()>;
}
