//! The connection task. It owns the [`Conn`] and runs one command at a time, so a dropped or
//! cancelled result stream never interrupts a packet half-read: the task notices the drop or the
//! cancel and cleans up itself (`KILL QUERY`, then reads to the end of the response, bounded by
//! [`DROP_GRACE`]) before it frees the session (M6).
//!
//! # How a statement runs
//! `COM_QUERY` with the statement (parameters inlined as inert literals), then the response is
//! read incrementally: rows are decoded into Arrow batches until a batch is full, the batch is
//! delivered, and nothing more is read until the consumer asks (TCP back-pressure pauses the
//! server; the driver raises the session's `net_write_timeout` to [`PAUSED_RESULT_TIMEOUT`] so
//! a paused grid keeps its result that long). Every result set of a `CALL` is read in turn.
//!
//! After the statement: the server's warnings become notices (`SHOW WARNINGS`), and the session
//! is checked: in a read-only session after every statement, otherwise after statements that
//! may change session modes, a read-back of `transaction_read_only`, `sql_mode` and the
//! character sets. A lifted read-only mode or a non-UTF-8 character set closes the session.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use ninesql_driver_api::{
    DriverError, DriverResult, ExecutionGuard, FetchBudget, Notice, NoticeSeverity, ResultItem,
    ResultSetHeader, SERVER_CANCEL_GRACE, StatementClass,
};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

use crate::cancel::{Control, ExecCancel};
use crate::conn::{
    COM_QUERY, Conn, Head, MyError, RowOrEnd, RowSink, STATUS_AUTOCOMMIT, STATUS_IN_TRANS,
    STATUS_MORE_RESULTS, STATUS_NO_BACKSLASH_ESCAPES, local_infile_error,
};
use crate::dialect::Modes;
use crate::types::{BatchBuilder, schema_for};

/// `net_write_timeout` set for the session (only ever raised): how long the server keeps a
/// result whose consumer paused (a grid not scrolled) before it closes the connection.
pub const PAUSED_RESULT_TIMEOUT: Duration = Duration::from_secs(300);

/// How long a dropped stream's statement may take to end after the driver cancelled it, or to
/// drain, before the session is closed.
const DROP_GRACE: Duration = SERVER_CANCEL_GRACE;

/// Bound on one driver statement (transaction control, settings, metadata, read-back).
pub(crate) const INTERNAL_TIMEOUT: Duration = Duration::from_secs(60);

/// Most server warnings turned into notices per statement.
const MAX_WARNINGS: usize = 1000;

/// State shared by the session, its metadata and the connection task.
#[derive(Debug)]
pub(crate) struct Shared {
    pub(crate) closed: AtomicBool,
    close_reason: Mutex<Option<String>>,
    /// Server status flags of the last OK / EOF packet.
    pub(crate) status: AtomicU16,
    /// The session is read-only (the engine-side setting must stay on).
    pub(crate) read_only: AtomicBool,
    /// `transaction_read_only`, or `tx_read_only` on older servers.
    pub(crate) ro_var: &'static str,
}

impl Shared {
    pub(crate) fn new(status: u16, ro_var: &'static str) -> Self {
        Self {
            closed: AtomicBool::new(false),
            close_reason: Mutex::new(None),
            status: AtomicU16::new(status),
            read_only: AtomicBool::new(false),
            ro_var,
        }
    }

    pub(crate) fn mark_closed(&self, reason: &str) {
        let mut r = self
            .close_reason
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if r.is_none() {
            *r = Some(reason.to_owned());
        }
        self.closed.store(true, Ordering::SeqCst);
    }

    pub(crate) fn closed_error(&self) -> DriverError {
        let r = self
            .close_reason
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        DriverError::Closed(r.clone().unwrap_or_else(|| "session closed".into()))
    }

    pub(crate) fn in_transaction(&self) -> bool {
        self.status.load(Ordering::SeqCst) & STATUS_IN_TRANS != 0
    }

    pub(crate) fn auto_commit(&self) -> bool {
        self.status.load(Ordering::SeqCst) & STATUS_AUTOCOMMIT != 0
    }
}

/// Items for one poll of the raw stream.
pub(crate) struct Chunk {
    pub(crate) items: Vec<DriverResult<ResultItem>>,
    pub(crate) done: bool,
}

/// A request for the next chunk; dropping it means the consumer is gone.
pub(crate) type Demand = oneshot::Sender<Chunk>;

/// One user statement.
pub(crate) struct ExecRequest {
    /// The statement with its parameters inlined.
    pub(crate) text: String,
    pub(crate) budget: FetchBudget,
    pub(crate) class: StatementClass,
    pub(crate) guard: ExecutionGuard,
    pub(crate) cancel: Arc<ExecCancel>,
    pub(crate) demands: mpsc::Receiver<Demand>,
}

pub(crate) type Rows = Vec<Vec<Option<String>>>;

pub(crate) enum Command {
    Execute(Box<ExecRequest>),
    /// A driver statement (transaction control, settings, metadata), text results. `verify`
    /// checks the session afterwards (always in read-only sessions).
    Internal {
        sql: String,
        /// Largest value kept: `None` cuts values at 64 KiB silently; `Some(n)` fails the
        /// statement when a value is longer than `n` (DDL text is never returned cut).
        value_limit: Option<u64>,
        verify: bool,
        guard: ExecutionGuard,
        reply: oneshot::Sender<DriverResult<Rows>>,
    },
    Ping {
        guard: ExecutionGuard,
        reply: oneshot::Sender<DriverResult<()>>,
    },
    Close {
        reply: oneshot::Sender<()>,
    },
}

/// Statement error → driver error.
pub(crate) fn statement_error(e: &MyError, cancelled: bool) -> DriverError {
    if e.is_fatal() {
        return DriverError::Closed(e.display());
    }
    match e.code {
        1792 => DriverError::ReadOnly(e.display()),
        // Interrupted (KILL QUERY), MySQL max_execution_time, MariaDB max_statement_time.
        1317 | 3024 | 1969 if cancelled => DriverError::Cancelled,
        _ if e.state == "25006" => DriverError::ReadOnly(e.display()),
        _ => DriverError::Query {
            code: (e.code != 0).then(|| e.code.to_string()),
            message: if e.state.is_empty() {
                e.message.clone()
            } else {
                format!("{} (SQLSTATE {})", e.message, e.state)
            },
            position: None,
        },
    }
}

/// Per-execution state.
struct Exec {
    guard: Option<ExecutionGuard>,
    cancel: Arc<ExecCancel>,
    demands: mpsc::Receiver<Demand>,
    class: StatementClass,
    items: Vec<DriverResult<ResultItem>>,
    builder: Option<BatchBuilder>,
    error: Option<MyError>,
    warnings: u32,
    rows_seen: u64,
    /// The driver itself sent `KILL QUERY` (consumer gone).
    self_cancelled: bool,
    /// Bound on the rest of the execution once the consumer is gone or cancelled.
    deadline: Option<Instant>,
}

impl Exec {
    fn cancelled(&self) -> bool {
        self.self_cancelled || self.cancel.is_requested()
    }

    fn may_cancel(&self) -> bool {
        self.class == StatementClass::Read || self.rows_seen == 0
    }

    fn flush_batch(&mut self) {
        let Some(b) = self.builder.as_mut() else {
            return;
        };
        for col in b.take_new_zero_date_columns() {
            self.items.push(Ok(ResultItem::Notice(Notice::new(
                NoticeSeverity::Warning,
                format!(
                    "column \"{col}\" holds zero or invalid dates, shown as NULL and flagged as \
                     not editable"
                ),
            ))));
        }
        match b.finish() {
            Ok(Some(batch)) => self.items.push(Ok(ResultItem::Rows(batch))),
            Ok(None) => {}
            Err(e) => self.items.push(Err(e)),
        }
    }
}

/// Polls `fut` (a read on the connection) to completion. Meanwhile, if the consumer waiting on
/// `reply` leaves, sends `KILL QUERY` (when `may_cancel`, awaited so it returned before the end
/// is confirmed, M6) and bounds the rest by [`DROP_GRACE`]; past the deadline the session is
/// closed. The read itself is never dropped half-way.
async fn watched<T>(
    mut fut: Pin<&mut impl Future<Output = DriverResult<T>>>,
    deadline: &mut Option<Instant>,
    self_cancelled: &mut bool,
    reply: &mut Option<Demand>,
    control: &Control,
    may_cancel: bool,
) -> DriverResult<T> {
    loop {
        let watching = deadline.is_none();
        let watch_reply = async {
            match reply.as_mut() {
                Some(r) if watching => r.closed().await,
                _ => std::future::pending().await,
            }
        };
        let limit = *deadline;
        let expired = async {
            match limit {
                Some(d) => tokio::time::sleep_until(d).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            biased;
            r = &mut fut => return r,
            () = watch_reply => {
                if may_cancel && !*self_cancelled {
                    *self_cancelled = true;
                    let _ = control.kill().await;
                }
                *deadline = Some(Instant::now() + DROP_GRACE);
            }
            () = expired => {
                return Err(DriverError::Closed(
                    "the server did not confirm the end of a dropped statement in time".into(),
                ));
            }
        }
    }
}

pub(crate) struct Actor {
    conn: Conn,
    shared: Arc<Shared>,
    modes: Arc<Modes>,
    control: Arc<Control>,
}

impl Actor {
    pub(crate) fn new(
        conn: Conn,
        shared: Arc<Shared>,
        modes: Arc<Modes>,
        control: Arc<Control>,
    ) -> Self {
        Self {
            conn,
            shared,
            modes,
            control,
        }
    }

    /// Runs commands until the session is dropped, closed or the connection is lost.
    pub(crate) async fn run(mut self, mut rx: mpsc::Receiver<Command>) {
        while let Some(cmd) = rx.recv().await {
            match cmd {
                Command::Execute(req) => self.execute(*req).await,
                Command::Internal {
                    sql,
                    value_limit,
                    verify,
                    guard,
                    reply,
                } => {
                    let mut result = self.internal_limited(&sql, value_limit).await;
                    if result.is_ok()
                        && let Err(e) = self.verify_session(verify).await
                    {
                        result = Err(e);
                    }
                    if let Err(e) = &result {
                        self.note_fatal(e);
                    }
                    guard.complete();
                    let _ = reply.send(result);
                }
                Command::Ping { guard, reply } => {
                    let result =
                        match tokio::time::timeout(INTERNAL_TIMEOUT, self.conn.ping()).await {
                            Ok(Ok(Ok(()))) => Ok(()),
                            Ok(Ok(Err(e))) => Err(statement_error(&e, false)),
                            Ok(Err(e)) => Err(e),
                            Err(_) => Err(DriverError::Closed("ping timed out".into())),
                        };
                    self.sync_status();
                    if let Err(e) = &result {
                        self.note_fatal(e);
                    }
                    guard.complete();
                    let _ = reply.send(result);
                }
                Command::Close { reply } => {
                    self.shutdown().await;
                    let _ = reply.send(());
                    return;
                }
            }
            if self.shared.closed.load(Ordering::SeqCst) {
                break;
            }
        }
        self.shutdown().await;
        tracing::debug!("mysqlwire connection task ended");
    }

    async fn shutdown(&mut self) {
        self.conn.quit().await;
        self.control.close().await;
        self.shared.mark_closed("session closed");
    }

    fn note_fatal(&self, e: &DriverError) {
        if e.is_fatal() {
            self.shared.mark_closed(&e.to_string());
        }
    }

    fn sync_status(&self) {
        let status = self.conn.status;
        self.shared.status.store(status, Ordering::SeqCst);
        self.modes
            .no_backslash_escapes
            .store(status & STATUS_NO_BACKSLASH_ESCAPES != 0, Ordering::SeqCst);
    }

    /// Runs a driver statement and returns its rows as text, bounded by
    /// [`INTERNAL_TIMEOUT`]: past it the connection is out of sync and the session is closed.
    pub(crate) async fn internal(&mut self, sql: &str) -> DriverResult<Rows> {
        self.internal_limited(sql, None).await
    }

    /// [`internal`](Self::internal) with a value limit (see `Command::Internal`).
    pub(crate) async fn internal_limited(
        &mut self,
        sql: &str,
        value_limit: Option<u64>,
    ) -> DriverResult<Rows> {
        let query = self.conn.query_limited(sql, value_limit);
        let r = match tokio::time::timeout(INTERNAL_TIMEOUT, query).await {
            Ok(r) => r,
            Err(_) => {
                self.close_session("driver statement timed out");
                return Err(DriverError::Closed(format!(
                    "a driver statement did not finish within {INTERNAL_TIMEOUT:?}; the session \
                     was closed"
                )));
            }
        };
        self.sync_status();
        match r? {
            Ok(rows) if rows.overflow => Err(DriverError::Query {
                code: None,
                message: "the listing is larger than the driver's 32 MiB limit; use a smaller \
                          page or a filter"
                    .into(),
                position: None,
            }),
            Ok(rows) if rows.cut && value_limit.is_some() => Err(DriverError::Query {
                code: None,
                message: format!(
                    "the definition is larger than {} KiB and was not loaded",
                    value_limit.unwrap_or(0) / 1024
                ),
                position: None,
            }),
            Ok(rows) => Ok(rows.rows),
            Err(e) => Err(statement_error(&e, false)),
        }
    }

    /// Closes the session: the connection task stops after the current command.
    fn close_session(&self, reason: &str) {
        tracing::warn!(reason, "mysqlwire session closed by the driver");
        self.shared.mark_closed(reason);
    }

    /// Checks the session: the character sets stay UTF-8 and a read-only session stays
    /// read-only; updates the lexical modes from `sql_mode`. Runs in read-only sessions always,
    /// otherwise when `force`. Any failure of the read-back closes the session (fails closed).
    async fn verify_session(&mut self, force: bool) -> DriverResult<()> {
        let read_only = self.shared.read_only.load(Ordering::SeqCst);
        if !read_only && !force {
            return Ok(());
        }
        let sql = format!(
            "SELECT @@SESSION.sql_mode, @@SESSION.character_set_client, \
             @@SESSION.character_set_connection, @@SESSION.character_set_results, \
             @@SESSION.{}",
            self.shared.ro_var
        );
        let mut rows = self.internal(&sql).await;
        // A late `KILL QUERY` can interrupt the read-back itself: try once more.
        if matches!(&rows, Err(DriverError::Query { code: Some(c), .. }) if c == "1317") {
            rows = self.internal(&sql).await;
        }
        let row = match rows {
            Ok(r) => r.into_iter().next().unwrap_or_default(),
            Err(e) => {
                tracing::warn!(error = ?std::mem::discriminant(&e), "session read-back failed");
                self.close_session("session state could not be verified");
                return Err(DriverError::Closed(
                    "the session state could not be verified after the statement; the session \
                     was closed"
                        .into(),
                ));
            }
        };
        let cell = |i: usize| row.get(i).cloned().flatten();
        if let Some(mode) = cell(0) {
            self.modes.set_sql_mode(&mode);
        }
        let utf8 = (1..=3).all(|i| cell(i).is_some_and(|c| c.starts_with("utf8")));
        if !utf8 {
            self.close_session("character set changed");
            return Err(DriverError::Closed(
                "the connection character set was changed; NineSQL requires utf8mb4, so the \
                 session was closed"
                    .into(),
            ));
        }
        if read_only && cell(4).as_deref() != Some("1") {
            self.close_session("read-only mode lifted");
            return Err(DriverError::ReadOnly(
                "a statement turned the session's read-only mode off; the session was closed"
                    .into(),
            ));
        }
        if read_only {
            // `SET TRANSACTION … READ WRITE` (no SESSION) only changes the *next* transaction
            // and leaves `transaction_read_only` at 1, so the read-back cannot see it.
            // Re-asserting the session characteristic resets that pending one-shot mode, on
            // MySQL and MariaDB, inside an open transaction too.
            let sql = "SET SESSION TRANSACTION READ ONLY";
            let mut r = self.internal(sql).await;
            if matches!(&r, Err(DriverError::Query { code: Some(c), .. }) if c == "1317") {
                r = self.internal(sql).await;
            }
            if let Err(e) = r {
                tracing::warn!(error = ?std::mem::discriminant(&e), "read-only re-assert failed");
                self.close_session("read-only mode could not be re-asserted");
                return Err(DriverError::Closed(
                    "the read-only mode could not be re-asserted after the statement; the \
                     session was closed"
                        .into(),
                ));
            }
        }
        Ok(())
    }

    /// Waits for the next demand. `Err(true)` = cancelled, `Err(false)` = consumer gone.
    async fn wait_demand(ex: &mut Exec) -> Result<Demand, bool> {
        if ex.cancel.is_requested() {
            return Err(true);
        }
        tokio::select! {
            biased;
            () = ex.cancel.requested() => Err(true),
            d = ex.demands.recv() => d.ok_or(false),
        }
    }

    /// Sends the final chunk, after the guard confirmed the end. When no demand is pending, a
    /// detached task answers the next one.
    fn finish(ex: &mut Exec, reply: Option<Demand>, terminal: Option<DriverError>) {
        if let Some(e) = terminal {
            ex.items.push(Err(e));
        }
        if let Some(g) = ex.guard.take() {
            g.complete();
        }
        let chunk = Chunk {
            items: std::mem::take(&mut ex.items),
            done: true,
        };
        match reply {
            Some(r) => {
                let _ = r.send(chunk);
            }
            None => {
                let mut demands = std::mem::replace(&mut ex.demands, mpsc::channel(1).1);
                tokio::spawn(async move {
                    if let Some(r) = demands.recv().await {
                        let _ = r.send(chunk);
                    }
                });
            }
        }
    }

    async fn execute(&mut self, req: ExecRequest) {
        let ExecRequest {
            text,
            budget,
            class,
            guard,
            cancel,
            demands,
        } = req;
        let mut ex = Exec {
            guard: Some(guard),
            cancel,
            demands,
            class,
            items: Vec::new(),
            builder: None,
            error: None,
            warnings: 0,
            rows_seen: 0,
            self_cancelled: false,
            deadline: None,
        };
        // Lazy: nothing is sent before the consumer polls.
        let mut reply = match Self::wait_demand(&mut ex).await {
            Ok(r) => Some(r),
            Err(true) => return Self::finish(&mut ex, None, Some(DriverError::Cancelled)),
            Err(false) => return Self::finish(&mut ex, None, None),
        };
        let outcome = match self.conn.command(COM_QUERY, text.as_bytes()).await {
            Ok(()) => self.responses(&mut ex, &mut reply, budget).await,
            Err(e) => Err(e),
        };
        self.sync_status();
        match outcome {
            Ok(()) => self.end(&mut ex, reply).await,
            Err(e) => {
                // Every error here comes from the connection: it is out of sync or gone.
                tracing::debug!(error = ?std::mem::discriminant(&e), "mysqlwire connection unusable");
                self.close_session("connection lost or unresponsive");
                let e = if matches!(e, DriverError::Closed(_)) {
                    e
                } else {
                    DriverError::Closed(e.to_string())
                };
                Self::finish(&mut ex, reply, Some(e));
            }
        }
    }

    /// Reads every response of the statement (result sets of a `CALL` in turn).
    async fn responses(
        &mut self,
        ex: &mut Exec,
        reply: &mut Option<Demand>,
        budget: FetchBudget,
    ) -> DriverResult<()> {
        let mut index = 0u32;
        loop {
            let may_cancel = ex.may_cancel();
            let head = {
                let fut = self.conn.read_head();
                tokio::pin!(fut);
                watched(
                    fut,
                    &mut ex.deadline,
                    &mut ex.self_cancelled,
                    reply,
                    &self.control,
                    may_cancel,
                )
                .await?
            };
            let status = match head {
                Head::Ok(ok) => {
                    ex.warnings = ex.warnings.saturating_add(u32::from(ok.warnings));
                    if ex.error.is_none() {
                        ex.items
                            .push(Ok(ResultItem::UpdateCount(Some(ok.affected_rows))));
                    }
                    ok.status
                }
                Head::Err(e) => {
                    ex.error.get_or_insert(e);
                    return Ok(());
                }
                Head::LocalInfile => {
                    self.conn.refuse_local_infile().await?;
                    ex.error.get_or_insert(local_infile_error());
                    continue;
                }
                Head::ResultSet(n) => {
                    let cols = {
                        let fut = self.conn.read_columns(n);
                        tokio::pin!(fut);
                        watched(
                            fut,
                            &mut ex.deadline,
                            &mut ex.self_cancelled,
                            reply,
                            &self.control,
                            may_cancel,
                        )
                        .await?
                    };
                    let (schema, kinds) = schema_for(&cols);
                    if ex.error.is_none() {
                        ex.items
                            .push(Ok(ResultItem::ResultSet(ResultSetHeader::tabular(
                                index,
                                Arc::clone(&schema),
                            ))));
                    }
                    index += 1;
                    ex.builder = Some(BatchBuilder::new(schema, kinds, budget));
                    let end = self.rows(ex, reply, cols.len()).await?;
                    ex.flush_batch();
                    ex.builder = None;
                    match end {
                        Some(status) => status,
                        None => return Ok(()),
                    }
                }
            };
            if status & STATUS_MORE_RESULTS == 0 {
                return Ok(());
            }
        }
    }

    /// Reads the rows of one result set; `Some(status)` at its end, `None` after an error.
    async fn rows(
        &mut self,
        ex: &mut Exec,
        reply: &mut Option<Demand>,
        ncols: usize,
    ) -> DriverResult<Option<u16>> {
        loop {
            // Deliver a full batch, then wait for the consumer (the server pauses meanwhile).
            if ex.builder.as_ref().is_some_and(BatchBuilder::is_full) && ex.deadline.is_none() {
                ex.flush_batch();
                if let Some(r) = reply.take() {
                    let chunk = Chunk {
                        items: std::mem::take(&mut ex.items),
                        done: false,
                    };
                    if r.send(chunk).is_err() {
                        self.abandon(ex, false).await;
                    }
                }
                if ex.deadline.is_none() {
                    match Self::wait_demand(ex).await {
                        Ok(r) => *reply = Some(r),
                        Err(cancelled) => self.abandon(ex, cancelled).await,
                    }
                }
            }
            if ex.cancel.is_requested() && ex.deadline.is_none() {
                // The handle sent `KILL QUERY`: read the rest without keeping it.
                ex.deadline = Some(Instant::now() + DROP_GRACE);
            }
            // A drain that keeps receiving rows never lets `watched` time out: check here too.
            if ex.deadline.is_some_and(|d| Instant::now() >= d) {
                return Err(DriverError::Closed(
                    "the server did not confirm the end of a dropped statement in time".into(),
                ));
            }
            let may_cancel = ex.may_cancel();
            let draining = ex.deadline.is_some();
            let read = {
                let sink = match ex.builder.as_mut() {
                    Some(b) if !draining => RowSink::Arrow(b),
                    _ => RowSink::Discard,
                };
                let fut = self.conn.read_row(ncols, sink);
                tokio::pin!(fut);
                watched(
                    fut,
                    &mut ex.deadline,
                    &mut ex.self_cancelled,
                    reply,
                    &self.control,
                    may_cancel,
                )
                .await?
            };
            match read {
                RowOrEnd::Row => ex.rows_seen += 1,
                RowOrEnd::End(eof) => {
                    ex.warnings = ex.warnings.saturating_add(u32::from(eof.warnings));
                    return Ok(Some(eof.status));
                }
                RowOrEnd::Err(e) => {
                    ex.error.get_or_insert(e);
                    return Ok(None);
                }
            }
        }
    }

    /// The consumer left (`cancelled = false`) or cancelled between batches: kill the statement
    /// when that is safe (reads, or before any row), and read the rest of the response under a
    /// deadline instead of keeping it.
    async fn abandon(&mut self, ex: &mut Exec, cancelled: bool) {
        if !cancelled && ex.may_cancel() && !ex.self_cancelled {
            ex.self_cancelled = true;
            let _ = self.control.kill().await;
        }
        if cancelled {
            ex.cancel.request();
        }
        ex.deadline = Some(Instant::now() + DROP_GRACE);
    }

    /// Ends the statement: warnings, session checks, the update count, confirms the end and
    /// sends the final chunk.
    async fn end(&mut self, ex: &mut Exec, reply: Option<Demand>) {
        ex.flush_batch();
        let cancelled = ex.cancelled();
        let mut terminal = match &ex.error {
            Some(e) => Some(statement_error(e, cancelled)),
            None if cancelled => Some(DriverError::Cancelled),
            None => None,
        };
        if let Some(DriverError::Closed(_)) = &terminal {
            self.close_session("the server closed the connection");
        }
        if !self.shared.closed.load(Ordering::SeqCst) {
            if ex.warnings > 0 && !cancelled && reply.is_some() {
                self.push_warnings(ex).await;
            }
            let force = !matches!(
                ex.class,
                StatementClass::Read
                    | StatementClass::Write
                    | StatementClass::Ddl
                    | StatementClass::Destructive
                    | StatementClass::Transaction
            );
            if let Err(e) = self.verify_session(force).await {
                terminal = Some(e);
            }
        }
        Self::finish(ex, reply, terminal);
    }

    /// The statement's warnings as notices (at most [`MAX_WARNINGS`]).
    async fn push_warnings(&mut self, ex: &mut Exec) {
        let sql = format!("SHOW WARNINGS LIMIT {MAX_WARNINGS}");
        match self.internal(&sql).await {
            Ok(rows) => {
                for row in rows.into_iter().take(MAX_WARNINGS) {
                    let level = row.first().cloned().flatten().unwrap_or_default();
                    let code = row.get(1).cloned().flatten();
                    let message = row.get(2).cloned().flatten().unwrap_or_default();
                    let severity = match level.as_str() {
                        "Warning" | "Error" => NoticeSeverity::Warning,
                        _ => NoticeSeverity::Info,
                    };
                    let mut n = Notice::new(severity, message);
                    if let Some(c) = code {
                        n = n.with_code(c);
                    }
                    ex.items.push(Ok(ResultItem::Notice(n)));
                }
            }
            Err(e) => {
                tracing::debug!(error = ?std::mem::discriminant(&e), "SHOW WARNINGS failed");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_map() {
        let e = |code: u16, state: &str| MyError {
            code,
            state: state.into(),
            message: "m".into(),
        };
        assert!(matches!(
            statement_error(&e(1792, "25006"), false),
            DriverError::ReadOnly(_)
        ));
        assert!(matches!(
            statement_error(&e(1317, "70100"), true),
            DriverError::Cancelled
        ));
        assert!(matches!(
            statement_error(&e(1317, "70100"), false),
            DriverError::Query { .. }
        ));
        assert!(matches!(
            statement_error(&e(1053, "08S01"), false),
            DriverError::Closed(_)
        ));
        match statement_error(&e(1062, "23000"), false) {
            DriverError::Query { code, message, .. } => {
                assert_eq!(code.as_deref(), Some("1062"));
                assert!(message.contains("23000"));
            }
            other => panic!("{other:?}"),
        }
    }
}
