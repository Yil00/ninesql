//! The connection task. It owns the [`Conn`] and runs one command at a time, so a dropped or
//! cancelled result stream never interrupts a message half-read: the task notices the drop or
//! the cancel and cleans up itself (closes the portal, ends the driver's implicit transaction,
//! reads up to `ReadyForQuery`) before it frees the session (M6, docs/08 idle-cursor rule).
//!
//! # How a statement runs
//! 1. `Close` + `Parse` (named statement) + `Describe` + `Sync`: the statement is parsed once and
//!    its result columns are known before anything runs (one statement per protocol call).
//! 2. Row-returning statements:
//!    - `SELECT`-class statements outside a transaction run in a **driver-implicit read
//!      transaction** (`BEGIN`, a transaction-local `idle_in_transaction_session_timeout`, only
//!      ever lowered) through a portal fetched `batch_rows` rows at a time (`Execute` + `Sync`
//!      per batch): nothing runs ahead of the consumer. The transaction is committed only when
//!      the portal ran to completion; a dropped stream, a cancel or an error closes the portal
//!      and rolls back.
//!    - inside the user's transaction, the same portal fetch without `BEGIN` / `COMMIT`; a cancel
//!      or drop closes the portal (`Close` + `Sync`).
//!    - other row-returning statements outside a transaction (`INSERT … RETURNING`, `CALL`, a
//!      `SELECT` the classifier flags) run in one `Execute` in PostgreSQL's own implicit
//!      transaction, read incrementally from the socket (TCP back-pressure bounds what is read).
//! 3. Other statements: `Bind` + `Execute` + `Sync`, then the update count.
//!
//! After the statement, the session is checked: the access mode (read-only sessions,
//! `default_transaction_read_only` from `ParameterStatus` on PostgreSQL ≥ 14, a read-back before)
//! and the client encoding (UTF-8 only). A change closes the session.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use bytes::BytesMut;
use ninesql_driver_api::{
    DriverError, DriverResult, ExecutionGuard, FetchBudget, Notice, NoticeSeverity, Param,
    ResultItem, ResultSetHeader, SERVER_CANCEL_GRACE, StatementClass,
};
use postgres_protocol::IsNull;
use postgres_protocol::message::{backend, frontend};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

use crate::cancel::{CancelTarget, ExecCancel, MetaGate};
use crate::conn::{Conn, Msg, PgError, RowSink};
use crate::types::{BatchBuilder, ColPlan, builtin_type_name, schema_for};

/// Name of the prepared statement the driver uses for user statements.
const STMT: &str = "ninesql_s";

/// `idle_in_transaction_session_timeout` set (transaction-local, with `set_config`) in the
/// driver-implicit read transaction of a paused cursor, unless a stricter non-zero value is
/// already in force: the server ends a session whose consumer never comes back, releasing the
/// snapshot and locks of the open portal. Documented in docs/08.
pub const IDLE_CURSOR_TIMEOUT: Duration = Duration::from_secs(300);

/// How long a dropped stream's statement may take to end after the driver cancelled it, or to
/// drain, before the session is closed.
const DROP_GRACE: Duration = SERVER_CANCEL_GRACE;

/// State shared by the session, its metadata and the connection task.
#[derive(Debug)]
pub(crate) struct Shared {
    pub(crate) closed: AtomicBool,
    close_reason: Mutex<Option<String>>,
    pub(crate) txn_status: AtomicU8,
    /// The session is read-only (the engine-side setting must stay on).
    pub(crate) read_only: AtomicBool,
    pub(crate) major_version: Option<u32>,
}

impl Shared {
    pub(crate) fn new(major_version: Option<u32>, txn_status: u8) -> Self {
        Self {
            closed: AtomicBool::new(false),
            close_reason: Mutex::new(None),
            txn_status: AtomicU8::new(txn_status),
            read_only: AtomicBool::new(false),
            major_version,
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
        self.txn_status.load(Ordering::SeqCst) != b'I'
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
    pub(crate) text: String,
    pub(crate) params: Vec<Param>,
    pub(crate) budget: FetchBudget,
    pub(crate) class: StatementClass,
    pub(crate) is_call: bool,
    /// Manual-commit mode: start a transaction first when none is open.
    pub(crate) auto_begin: bool,
    pub(crate) guard: ExecutionGuard,
    pub(crate) cancel: Arc<ExecCancel>,
    pub(crate) demands: mpsc::Receiver<Demand>,
}

pub(crate) type Rows = Vec<Vec<Option<String>>>;

pub(crate) enum Command {
    Execute(Box<ExecRequest>),
    /// A driver statement (transaction control, settings, metadata), text results.
    Internal {
        sql: String,
        params: Vec<Option<String>>,
        max_rows: i32,
        /// Largest value kept: `None` cuts values at 64 KiB silently; `Some(n)` fails the
        /// statement when a value is longer than `n` (DDL text is never returned cut).
        value_limit: Option<usize>,
        /// Set for navigator statements: run inside a savepoint when a transaction is open,
        /// cancellable only while the statement runs (security review M1).
        gate: Option<Arc<MetaGate>>,
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

pub(crate) struct Actor {
    conn: Conn,
    shared: Arc<Shared>,
    cancel_target: Arc<CancelTarget>,
    type_names: HashMap<u32, String>,
}

/// Where the portal of a row-returning statement is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fetch {
    /// `ReadyForQuery` received after `PortalSuspended`: the next `Execute` may be sent.
    Suspended,
    /// Messages of an `Execute` are still to be read.
    InResponse,
    /// The portal completed (or failed); `ReadyForQuery` received.
    Finished,
}

/// Per-execution state of a row-returning statement.
struct Exec {
    guard: Option<ExecutionGuard>,
    cancel: Arc<ExecCancel>,
    demands: mpsc::Receiver<Demand>,
    class: StatementClass,
    /// The driver opened the transaction (`BEGIN`) and ends it.
    implicit_txn: bool,
    /// A portal fetched batch by batch (otherwise one `Execute` for everything).
    paged: bool,
    batch_rows: i32,
    next_rows: i32,
    builder: Option<BatchBuilder>,
    items: Vec<DriverResult<ResultItem>>,
    /// `CommandComplete`s of the driver's `BEGIN` / timeout setting still expected (their rows
    /// are discarded).
    pre_completes: u8,
    fetch: Fetch,
    awaiting_ready: bool,
    tag: Option<String>,
    error: Option<PgError>,
    copy_out: bool,
    /// Notices queued in the current chunk, and their bytes.
    chunk_notices: u32,
    chunk_notice_bytes: usize,
    /// Notices dropped above the hard cap (reported once, at the end).
    notices_dropped: u64,
    rows_seen: u64,
    /// The driver itself sent a cancel request (consumer gone during a round trip).
    self_cancelled: bool,
    /// Bound on the rest of the execution once the consumer is gone.
    deadline: Option<Instant>,
}

/// Notices per chunk before the chunk is delivered.
const CHUNK_NOTICES: u32 = 1000;
/// Notice bytes per chunk before the chunk is delivered.
const CHUNK_NOTICE_BYTES: usize = 1024 * 1024;

impl Exec {
    /// Queues a notice. `true` when the chunk should be delivered now (soft cap). Where the
    /// chunk cannot be delivered (driver round trips), notices above twice the cap are dropped
    /// and counted.
    fn push_notice(&mut self, n: &PgError) -> bool {
        let size = n.message.len() + n.hint.as_ref().map_or(0, String::len) + 64;
        if self.chunk_notices >= 2 * CHUNK_NOTICES
            || self.chunk_notice_bytes + size > 2 * CHUNK_NOTICE_BYTES
        {
            self.notices_dropped += 1;
            return true;
        }
        self.chunk_notices += 1;
        self.chunk_notice_bytes += size;
        self.items.push(Ok(notice_item(n)));
        self.chunk_notices >= CHUNK_NOTICES || self.chunk_notice_bytes >= CHUNK_NOTICE_BYTES
    }

    fn cancelled(&self) -> bool {
        self.self_cancelled || self.cancel.is_requested()
    }

    fn flush_batch(&mut self) {
        let Some(b) = self.builder.as_mut() else {
            return;
        };
        let rows = b.rows();
        for col in b.take_new_infinite_columns() {
            self.items.push(Ok(ResultItem::Notice(Notice::new(
                NoticeSeverity::Warning,
                format!(
                    "column \"{col}\" holds infinite dates or timestamps, shown as NULL and \
                     flagged as not editable"
                ),
            ))));
        }
        match b.finish() {
            Ok(Some(batch)) => {
                // Adapt the next fetch so one `Execute` fills about one batch by bytes too.
                let bytes = batch.get_array_memory_size() as u64;
                if rows > 0 && self.paged {
                    let per_row = (bytes / u64::from(rows)).max(1);
                    let fit = b.budget().effective_max_batch_bytes() / per_row;
                    let fit = i32::try_from(fit).unwrap_or(i32::MAX);
                    self.next_rows = fit.clamp(1, self.batch_rows);
                }
                self.items.push(Ok(ResultItem::Rows(batch)));
            }
            Ok(None) => {}
            Err(e) => self.items.push(Err(e)),
        }
    }
}

/// The parts of [`Exec`] that [`Actor::read_watched`] updates.
struct Watch<'a> {
    deadline: &'a mut Option<Instant>,
    self_cancelled: &'a mut bool,
}

impl Exec {
    fn watch(&mut self) -> Watch<'_> {
        Watch {
            deadline: &mut self.deadline,
            self_cancelled: &mut self.self_cancelled,
        }
    }
}

/// Encodes a parameter in PostgreSQL text format.
fn encode_param(
    p: &Param,
    buf: &mut BytesMut,
) -> Result<IsNull, Box<dyn std::error::Error + Sync + Send>> {
    use bytes::BufMut;
    let text = match p {
        Param::Null => return Ok(IsNull::Yes),
        Param::Bool(b) => if *b { "t" } else { "f" }.to_owned(),
        Param::Int(i) => i.to_string(),
        Param::Float(f) if f.is_nan() => "NaN".to_owned(),
        Param::Float(f) if f.is_infinite() => {
            if *f > 0.0 { "Infinity" } else { "-Infinity" }.to_owned()
        }
        Param::Float(f) => format!("{f:?}"),
        Param::Decimal(d) => d.clone(),
        Param::Text(s) | Param::Json(s) => {
            buf.put_slice(s.as_bytes());
            return Ok(IsNull::No);
        }
        Param::Bytes(b) => {
            const DIGITS: &[u8; 16] = b"0123456789abcdef";
            buf.put_slice(b"\\x");
            for byte in b {
                buf.put_u8(DIGITS[usize::from(byte >> 4)]);
                buf.put_u8(DIGITS[usize::from(byte & 0x0f)]);
            }
            return Ok(IsNull::No);
        }
        Param::Date(days) => {
            let (y, m, d) = civil_from_days(i64::from(*days));
            format!("{y:04}-{m:02}-{d:02}")
        }
        Param::Timestamp { micros, utc } => {
            let secs = micros.div_euclid(1_000_000);
            let frac = micros.rem_euclid(1_000_000);
            let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
            let sod = secs.rem_euclid(86_400);
            let tz = if *utc { "+00" } else { "" };
            format!(
                "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}.{frac:06}{tz}",
                sod / 3600,
                (sod % 3600) / 60,
                sod % 60
            )
        }
        _ => return Err("unsupported parameter type".into()),
    };
    buf.put_slice(text.as_bytes());
    Ok(IsNull::No)
}

/// `true` when every parameter can be encoded.
pub(crate) fn params_supported(params: &[Param]) -> bool {
    params.iter().all(|p| {
        matches!(
            p,
            Param::Null
                | Param::Bool(_)
                | Param::Int(_)
                | Param::Float(_)
                | Param::Decimal(_)
                | Param::Text(_)
                | Param::Json(_)
                | Param::Bytes(_)
                | Param::Date(_)
                | Param::Timestamp { .. }
        )
    })
}

fn param_oid(p: &Param) -> u32 {
    // Only `bytea` is typed; other types are inferred by the server from the statement.
    if matches!(p, Param::Bytes(_)) { 17 } else { 0 }
}

/// Days since 1970-01-01 → (year, month, day), proleptic Gregorian (H. Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = u32::try_from(doy - (153 * mp + 2) / 5 + 1).unwrap_or(1);
    let m = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap_or(1);
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Statement error → driver error.
fn statement_error(e: &PgError, cancelled: bool) -> DriverError {
    if e.is_fatal() {
        return DriverError::Closed(e.display());
    }
    match e.code.as_str() {
        "25006" => DriverError::ReadOnly(e.display()),
        "57014" if cancelled => DriverError::Cancelled,
        _ => DriverError::Query {
            code: Some(e.code.clone()).filter(|c| !c.is_empty()),
            message: e.display(),
            position: e.position,
        },
    }
}

fn notice_item(n: &PgError) -> ResultItem {
    let severity = match n.severity.as_str() {
        "WARNING" => NoticeSeverity::Warning,
        "DEBUG" | "LOG" => NoticeSeverity::Debug,
        _ => NoticeSeverity::Info,
    };
    let mut notice = Notice::new(severity, n.display());
    if !n.code.is_empty() {
        notice = notice.with_code(n.code.clone());
    }
    ResultItem::Notice(notice)
}

/// Rows reported by a command tag (`INSERT 0 5`, `UPDATE 3`, `SELECT 10`…).
fn tag_count(tag: &str) -> Option<u64> {
    let mut words = tag.split_whitespace();
    let command = words.next()?;
    let last = tag.split_whitespace().last()?;
    match command {
        "INSERT" | "UPDATE" | "DELETE" | "MERGE" | "COPY" | "SELECT" | "MOVE" | "FETCH" => {
            last.parse().ok()
        }
        _ => None,
    }
}

/// Appends `Parse` + `Bind` + `Execute` of a fixed driver statement (unnamed statement and
/// portal) to `out`.
fn push_fixed(out: &mut BytesMut, sql: &str) -> DriverResult<()> {
    let proto = |e: std::io::Error| DriverError::InvalidParams(e.to_string());
    frontend::parse("", sql, std::iter::empty(), out).map_err(proto)?;
    frontend::bind(
        "",
        "",
        std::iter::empty(),
        std::iter::empty::<&Param>(),
        encode_param,
        std::iter::empty(),
        out,
    )
    .map_err(|_| DriverError::InvalidParams("cannot encode a driver statement".into()))?;
    frontend::execute("", 0, out).map_err(proto)
}

impl Actor {
    pub(crate) fn new(conn: Conn, shared: Arc<Shared>, cancel_target: Arc<CancelTarget>) -> Self {
        Self {
            conn,
            shared,
            cancel_target,
            type_names: HashMap::new(),
        }
    }

    /// Runs commands until the session is dropped, closed or the connection is lost.
    pub(crate) async fn run(mut self, mut rx: mpsc::Receiver<Command>) {
        while let Some(cmd) = rx.recv().await {
            match cmd {
                Command::Execute(req) => self.execute(*req).await,
                Command::Internal {
                    sql,
                    params,
                    max_rows,
                    value_limit,
                    gate,
                    verify,
                    guard,
                    reply,
                } => {
                    let mut result = match &gate {
                        Some(g) => {
                            self.navigator_statement(&sql, &params, max_rows, value_limit, g)
                                .await
                        }
                        None => {
                            self.internal_limited(&sql, &params, max_rows, value_limit)
                                .await
                        }
                    };
                    if result.is_ok()
                        && verify
                        && let Err(e) = self.verify_session().await
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
                    let result = self.ping().await;
                    if let Err(e) = &result {
                        self.note_fatal(e);
                    }
                    guard.complete();
                    let _ = reply.send(result);
                }
                Command::Close { reply } => {
                    self.conn.terminate().await;
                    self.shared.mark_closed("session closed");
                    let _ = reply.send(());
                    return;
                }
            }
            if self.shared.closed.load(Ordering::SeqCst) {
                break;
            }
        }
        self.conn.terminate().await;
        self.shared.mark_closed("session closed");
        tracing::debug!("pgwire connection task ended");
    }

    fn note_fatal(&self, e: &DriverError) {
        if e.is_fatal() {
            self.shared.mark_closed(&e.to_string());
        }
    }

    fn sync_status(&self) {
        self.shared
            .txn_status
            .store(self.conn.txn_status, Ordering::SeqCst);
    }

    async fn ping(&mut self) -> DriverResult<()> {
        frontend::sync(self.conn.out());
        self.conn.flush().await?;
        loop {
            match self.conn.next(RowSink::None).await? {
                Msg::Other(backend::Message::ReadyForQuery(_)) => {
                    self.sync_status();
                    return Ok(());
                }
                Msg::Error(e) => return Err(statement_error(&e, false)),
                _ => {}
            }
        }
    }

    /// Runs a driver statement with text parameters and results.
    pub(crate) async fn internal(
        &mut self,
        sql: &str,
        params: &[Option<String>],
        max_rows: i32,
    ) -> DriverResult<Rows> {
        self.internal_limited(sql, params, max_rows, None).await
    }

    /// A navigator statement. In an open transaction it runs inside `SAVEPOINT ninesql_meta`,
    /// released on success and rolled back to on any error or cancel, so neither a server
    /// error nor the cancel of an abandoned call aborts the user's transaction (security
    /// review M1). Savepoints are allowed in read-only transactions.
    async fn navigator_statement(
        &mut self,
        sql: &str,
        params: &[Option<String>],
        max_rows: i32,
        value_limit: Option<usize>,
        gate: &MetaGate,
    ) -> DriverResult<Rows> {
        const SAVEPOINT: &str = "SAVEPOINT ninesql_meta";
        const RELEASE: &str = "RELEASE SAVEPOINT ninesql_meta";
        const ROLLBACK_TO: &str = "ROLLBACK TO SAVEPOINT ninesql_meta";
        let in_tx = self.conn.txn_status == b'T';
        if in_tx {
            self.internal(SAVEPOINT, &[], 0).await?;
        }
        let result = if gate.start() {
            self.internal_limited(sql, params, max_rows, value_limit)
                .await
        } else {
            Err(DriverError::Cancelled)
        };
        gate.stop();
        if !in_tx {
            return result;
        }
        // A cancel request sent while the statement ran can reach one of these statements
        // instead: try again (the request is sent once).
        for _ in 0..3 {
            let step = if result.is_ok() && self.conn.txn_status == b'T' {
                self.internal(RELEASE, &[], 0).await.map(|_| ())
            } else {
                match self.internal(ROLLBACK_TO, &[], 0).await {
                    Ok(_) => self.internal(RELEASE, &[], 0).await.map(|_| ()),
                    Err(e) => Err(e),
                }
            };
            match step {
                Ok(()) => return result,
                Err(e) if e.is_fatal() => return Err(e),
                Err(_) => {}
            }
        }
        tracing::warn!("the navigator savepoint could not be released");
        Err(DriverError::Query {
            code: None,
            message: "the navigator could not restore the open transaction".into(),
            position: None,
        })
    }

    /// [`internal`](Self::internal) with a value limit (see `Command::Internal`).
    pub(crate) async fn internal_limited(
        &mut self,
        sql: &str,
        params: &[Option<String>],
        max_rows: i32,
        value_limit: Option<usize>,
    ) -> DriverResult<Rows> {
        let out = self.conn.out();
        let proto = |e: std::io::Error| DriverError::Protocol(e.to_string());
        frontend::parse("", sql, params.iter().map(|_| 0), out).map_err(proto)?;
        frontend::bind(
            "",
            "",
            std::iter::once(0),
            params.iter(),
            |p, buf| {
                Ok(match p {
                    Some(v) => {
                        bytes::BufMut::put_slice(buf, v.as_bytes());
                        IsNull::No
                    }
                    None => IsNull::Yes,
                })
            },
            std::iter::once(0),
            out,
        )
        .map_err(|_| DriverError::Protocol("cannot encode internal parameters".into()))?;
        frontend::execute("", max_rows, out).map_err(proto)?;
        frontend::sync(out);
        self.conn.flush().await?;
        let mut rows = crate::conn::TextRows::default();
        rows.value_limit = value_limit;
        let mut error = None;
        loop {
            match self.conn.next(RowSink::Text(&mut rows)).await? {
                Msg::Other(backend::Message::ReadyForQuery(_)) => break,
                Msg::Error(e) if error.is_none() => error = Some(e),
                _ => {}
            }
        }
        self.sync_status();
        match error {
            Some(e) => Err(statement_error(&e, false)),
            None if rows.overflow => Err(internal_overflow()),
            None if rows.cut && value_limit.is_some() => Err(DriverError::Query {
                code: None,
                message: format!(
                    "the definition is larger than {} KiB and was not loaded",
                    value_limit.unwrap_or(0) / 1024
                ),
                position: None,
            }),
            None => Ok(rows.rows),
        }
    }

    /// Closes the session: the connection task stops after the current command.
    fn close_session(&self, reason: &str) {
        tracing::warn!(reason, "pgwire session closed by the driver");
        self.shared.mark_closed(reason);
    }

    /// Checks the session after a statement: the client encoding stays UTF-8 and a read-only
    /// session stays read-only. Closes the session otherwise.
    async fn verify_session(&mut self) -> DriverResult<()> {
        if let Some(enc) = self.conn.server.get("client_encoding")
            && !enc.eq_ignore_ascii_case("UTF8")
        {
            let enc = enc.to_owned();
            self.close_session("client encoding changed");
            return Err(DriverError::Closed(format!(
                "the client encoding was changed to {enc}; NineSQL requires UTF8, so the \
                 session was closed"
            )));
        }
        if !self.shared.read_only.load(Ordering::SeqCst) {
            return Ok(());
        }
        let on = match self.shared.major_version {
            Some(v) if v >= 14 => {
                self.conn.server.get("default_transaction_read_only") == Some("on")
            }
            // Before 14 the setting is not reported: read it back (not possible in an aborted
            // transaction, where nothing can be written either; the next statement checks).
            _ if self.conn.txn_status == b'E' => true,
            // Any failure of the read-back fails closed.
            _ => match self
                .internal("SHOW default_transaction_read_only", &[], 1)
                .await
            {
                Ok(rows) => {
                    rows.first()
                        .and_then(|r| r.first())
                        .and_then(Option::as_deref)
                        == Some("on")
                }
                Err(e) => {
                    tracing::warn!(error = ?std::mem::discriminant(&e), "read-only read-back failed");
                    false
                }
            },
        };
        if !on {
            self.close_session("read-only mode lifted");
            return Err(DriverError::ReadOnly(
                "a statement turned the session's read-only mode off; the session was closed"
                    .into(),
            ));
        }
        // `SET TRANSACTION … READ WRITE` makes the open transaction writable without touching
        // `default_transaction_read_only`, so the check above cannot see it. The classifier
        // refuses its known forms (the comma form included) client-side; this re-assert is
        // defence in depth against classifier gaps. Inside a transaction block, re-assert
        // read-only for the transaction (always allowed: only going read-write is restricted).
        if self.conn.txn_status == b'T'
            && let Err(e) = self.internal("SET TRANSACTION READ ONLY", &[], 0).await
        {
            tracing::warn!(error = ?std::mem::discriminant(&e), "read-only re-assert failed");
            self.close_session("read-only mode could not be re-asserted");
            return Err(DriverError::Closed(
                "the read-only mode could not be re-asserted after the statement; the \
                     session was closed"
                    .into(),
            ));
        }
        Ok(())
    }

    /// Engine type name for `oid` (`ninesql.type` metadata).
    fn type_name(&self, oid: u32) -> String {
        builtin_type_name(oid)
            .map(str::to_owned)
            .or_else(|| self.type_names.get(&oid).cloned())
            .unwrap_or_else(|| format!("oid:{oid}"))
    }

    /// Looks up the names of OIDs not known yet (enums, domains, extension types).
    async fn resolve_type_names(&mut self, plans: &[ColPlan]) -> DriverResult<()> {
        let mut missing: Vec<u32> = plans
            .iter()
            .map(|p| p.oid)
            .filter(|o| builtin_type_name(*o).is_none() && !self.type_names.contains_key(o))
            .collect();
        missing.sort_unstable();
        missing.dedup();
        if missing.is_empty() || self.conn.txn_status == b'E' {
            return Ok(());
        }
        let list = missing
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let rows = self
            .internal(
                "SELECT t.oid::pg_catalog.text, pg_catalog.format_type(t.oid, NULL) \
                 FROM pg_catalog.pg_type t \
                 WHERE t.oid OPERATOR(pg_catalog.=) ANY($1::pg_catalog.oid[])",
                &[Some(format!("{{{list}}}"))],
                i32::try_from(missing.len()).unwrap_or(i32::MAX),
            )
            .await?;
        for row in rows {
            if let (Some(Some(oid)), Some(Some(name))) = (row.first(), row.get(1))
                && let Ok(oid) = oid.parse()
            {
                self.type_names.insert(oid, name.clone());
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

    /// Reads one message; while a consumer waits on `reply`, notices it leaving: then sends a
    /// cancel request (when `may_cancel`) and bounds the rest of the execution by `deadline`.
    async fn read_watched(
        conn: &mut Conn,
        sink: RowSink<'_>,
        watch: Watch<'_>,
        reply: &mut Option<Demand>,
        target: &Arc<CancelTarget>,
        may_cancel: bool,
    ) -> DriverResult<Msg> {
        let Watch {
            deadline,
            self_cancelled,
        } = watch;
        let fut = conn.next(sink);
        tokio::pin!(fut);
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
                        // Awaited (bounded by the grace period), never detached: the request
                        // must have returned before the end is confirmed, or it could reach the
                        // next statement (M6). The pending read keeps its state meanwhile.
                        let _ = target.send().await;
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

    /// Sends the buffered messages and reads up to `ReadyForQuery`, collecting notices and the
    /// first error (no rows expected).
    async fn fixed_round_trip(&mut self, ex: &mut Exec) -> DriverResult<Option<PgError>> {
        self.conn.flush().await?;
        let mut error = None;
        let mut none = None;
        loop {
            let msg = Self::read_watched(
                &mut self.conn,
                RowSink::Discard,
                ex.watch(),
                &mut none,
                &self.cancel_target,
                false,
            )
            .await?;
            match msg {
                Msg::Other(backend::Message::ReadyForQuery(_)) => break,
                Msg::Error(e) => {
                    if error.is_none() {
                        error = Some(e);
                    }
                }
                Msg::Notice(n) => {
                    ex.push_notice(&n);
                }
                _ => {}
            }
        }
        self.sync_status();
        Ok(error)
    }

    /// Sends the final chunk, after the guard confirmed the end. When no demand is pending, a
    /// detached task answers the next one.
    fn finish(&self, ex: &mut Exec, reply: Option<Demand>, terminal: Option<DriverError>) {
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

    /// Ends an execution whose connection broke: the session is closed.
    fn broken(&self, ex: &mut Exec, reply: Option<Demand>, e: DriverError) {
        tracing::debug!(error = %e, "pgwire connection unusable");
        self.close_session("connection lost or unresponsive");
        let e = if matches!(e, DriverError::Closed(_)) {
            e
        } else {
            DriverError::Closed(e.to_string())
        };
        self.finish(ex, reply, Some(e));
    }

    async fn execute(&mut self, req: ExecRequest) {
        let ExecRequest {
            text,
            params,
            budget,
            class,
            is_call,
            auto_begin,
            guard,
            cancel,
            demands,
        } = req;
        let batch_rows = i32::try_from(budget.effective_batch_rows()).unwrap_or(i32::MAX);
        let mut ex = Exec {
            guard: Some(guard),
            cancel,
            demands,
            class,
            implicit_txn: false,
            paged: false,
            batch_rows,
            next_rows: batch_rows,
            builder: None,
            items: Vec::new(),
            pre_completes: 0,
            fetch: Fetch::InResponse,
            awaiting_ready: false,
            tag: None,
            error: None,
            copy_out: false,
            chunk_notices: 0,
            chunk_notice_bytes: 0,
            notices_dropped: 0,
            rows_seen: 0,
            self_cancelled: false,
            deadline: None,
        };
        // Lazy: nothing is sent before the consumer polls.
        let mut reply = match Self::wait_demand(&mut ex).await {
            Ok(r) => Some(r),
            Err(true) => return self.finish(&mut ex, None, Some(DriverError::Cancelled)),
            Err(false) => return self.finish(&mut ex, None, None),
        };
        match self
            .run_statement(
                &text, &params, budget, is_call, auto_begin, &mut ex, &mut reply,
            )
            .await
        {
            Ok(()) => {}
            Err(e) if e.is_fatal() => self.broken(&mut ex, reply, e),
            Err(e) => {
                // A local error before the messages were sent (encoding): nothing reached the
                // server, the connection is still in sync.
                self.conn.out().clear();
                self.finish(&mut ex, reply, Some(e));
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_statement(
        &mut self,
        text: &str,
        params: &[Param],
        budget: FetchBudget,
        is_call: bool,
        auto_begin: bool,
        ex: &mut Exec,
        reply: &mut Option<Demand>,
    ) -> DriverResult<()> {
        // 1. Parse and describe.
        {
            let proto = |_: std::io::Error| {
                DriverError::InvalidParams("the statement contains a NUL character".into())
            };
            let begin = auto_begin && self.conn.txn_status == b'I';
            let out = self.conn.out();
            if begin {
                push_fixed(out, "BEGIN")?;
            }
            frontend::close(b'S', STMT, out).map_err(proto)?;
            frontend::parse(STMT, text, params.iter().map(param_oid), out).map_err(proto)?;
            frontend::describe(b'S', STMT, out).map_err(proto)?;
            frontend::sync(out);
        }
        self.conn.flush().await?;
        let mut plans: Option<Vec<ColPlan>> = None;
        let mut error = None;
        loop {
            let msg = Self::read_watched(
                &mut self.conn,
                RowSink::None,
                ex.watch(),
                reply,
                &self.cancel_target,
                true,
            )
            .await?;
            match msg {
                Msg::Other(backend::Message::RowDescription(rd)) => {
                    let mut v = Vec::new();
                    let mut fields = rd.fields();
                    use fallible_iterator::FallibleIterator as _;
                    while let Some(f) = fields
                        .next()
                        .map_err(|e| DriverError::Protocol(e.to_string()))?
                    {
                        v.push(ColPlan::new(f.name().to_owned(), f.type_oid()));
                    }
                    plans = Some(v);
                }
                Msg::Other(backend::Message::ReadyForQuery(_)) => break,
                Msg::Error(e) => {
                    if error.is_none() {
                        error = Some(e);
                    }
                }
                Msg::Notice(n) => {
                    ex.push_notice(&n);
                }
                _ => {}
            }
        }
        self.sync_status();
        if let Some(e) = error {
            return self.end(ex, reply.take(), Some(e)).await;
        }
        if ex.deadline.is_some() || ex.cancel.is_requested() {
            // The consumer left (or cancelled) while the statement was being parsed: it is not
            // executed.
            return self.end(ex, reply.take(), None).await;
        }
        if let Some(plans) = plans {
            self.resolve_type_names(&plans).await?;
            let names: Vec<String> = plans.iter().map(|p| self.type_name(p.oid)).collect();
            let lookup = |oid: u32| {
                plans
                    .iter()
                    .position(|p| p.oid == oid)
                    .map_or_else(|| format!("oid:{oid}"), |i| names[i].clone())
            };
            let schema = schema_for(&plans, &lookup);
            let formats: Vec<i16> = plans.iter().map(|p| p.kind.format()).collect();
            let plans: Arc<[ColPlan]> = plans.into();
            ex.items
                .push(Ok(ResultItem::ResultSet(ResultSetHeader::tabular(
                    0,
                    Arc::clone(&schema),
                ))));
            ex.builder = Some(BatchBuilder::new(schema, plans, budget));
            let outside_txn = self.conn.txn_status == b'I';
            ex.implicit_txn = outside_txn && ex.class == StatementClass::Read && !is_call;
            ex.paged = !outside_txn || ex.implicit_txn;
            self.bind_execute(params, &formats, ex)?;
            self.conn.flush().await?;
            self.fetch_loop(ex, reply).await
        } else {
            self.bind_execute(params, &[], ex)?;
            self.conn.flush().await?;
            ex.paged = false;
            self.fetch_loop(ex, reply).await
        }
    }

    fn bind_execute(
        &mut self,
        params: &[Param],
        formats: &[i16],
        ex: &mut Exec,
    ) -> DriverResult<()> {
        let proto = |e: std::io::Error| DriverError::InvalidParams(e.to_string());
        let out = self.conn.out();
        if ex.implicit_txn {
            push_fixed(out, "BEGIN")?;
            // Transaction-local, and only ever lowered: a stricter non-zero value already set
            // for the session or the role is kept. Its one result row is discarded.
            let ms = IDLE_CURSOR_TIMEOUT.as_millis();
            push_fixed(
                out,
                &format!(
                    "SELECT pg_catalog.set_config('idle_in_transaction_session_timeout'::pg_catalog.text, \
                     CASE WHEN s.setting::pg_catalog.int8 OPERATOR(pg_catalog.>) 0::pg_catalog.int8 \
                     AND s.setting::pg_catalog.int8 OPERATOR(pg_catalog.<) {ms}::pg_catalog.int8 \
                     THEN s.setting ELSE '{ms}'::pg_catalog.text END, true) \
                     FROM pg_catalog.pg_settings s WHERE s.name OPERATOR(pg_catalog.=) \
                     'idle_in_transaction_session_timeout'::pg_catalog.text"
                ),
            )?;
            ex.pre_completes = 2;
        }
        frontend::bind(
            "",
            STMT,
            std::iter::once(0),
            params.iter(),
            encode_param,
            formats.iter().copied(),
            out,
        )
        .map_err(|_| DriverError::Unsupported(ninesql_driver_api::Feature::Parameters))?;
        let n = if ex.paged { ex.next_rows } else { 0 };
        frontend::execute("", n, out).map_err(proto)?;
        frontend::sync(out);
        ex.fetch = Fetch::InResponse;
        ex.awaiting_ready = false;
        Ok(())
    }

    /// Streams the result: one chunk per demand.
    async fn fetch_loop(&mut self, ex: &mut Exec, reply: &mut Option<Demand>) -> DriverResult<()> {
        loop {
            // Fill the current demand.
            let ready = self.fill(ex, reply).await?;
            if ready == Filled::Finished {
                let error = ex.error.take();
                return self.end(ex, reply.take(), error).await;
            }
            // Deliver, then wait for the next demand (the portal is paused meanwhile).
            if let Some(r) = reply.take() {
                let chunk = Chunk {
                    items: std::mem::take(&mut ex.items),
                    done: false,
                };
                ex.chunk_notices = 0;
                ex.chunk_notice_bytes = 0;
                if r.send(chunk).is_err() {
                    return self.abandon(ex, false).await;
                }
            }
            match Self::wait_demand(ex).await {
                Ok(r) => *reply = Some(r),
                Err(cancelled) => return self.abandon(ex, cancelled).await,
            }
        }
    }

    /// Reads until a batch is ready or the statement finished.
    async fn fill(&mut self, ex: &mut Exec, reply: &mut Option<Demand>) -> DriverResult<Filled> {
        loop {
            if ex.fetch == Fetch::Suspended {
                if ex.cancel.is_requested() || ex.deadline.is_some() {
                    // Cancelled, or the consumer is gone: `end` closes the suspended portal
                    // instead of fetching more rows (idle-cursor rule).
                    return Ok(Filled::Finished);
                }
                let out = self.conn.out();
                frontend::execute("", ex.next_rows, out)
                    .map_err(|e| DriverError::Protocol(e.to_string()))?;
                frontend::sync(out);
                self.conn.flush().await?;
                ex.fetch = Fetch::InResponse;
            }
            let may_cancel = ex.class == StatementClass::Read || ex.rows_seen == 0;
            let sink = match ex.builder.as_mut() {
                // Rows of the driver's own pre-statements (the `set_config` above).
                _ if ex.pre_completes > 0 => RowSink::Discard,
                Some(b) => RowSink::Arrow(b),
                None => RowSink::None,
            };
            let watch = Watch {
                deadline: &mut ex.deadline,
                self_cancelled: &mut ex.self_cancelled,
            };
            let msg = Self::read_watched(
                &mut self.conn,
                sink,
                watch,
                reply,
                &self.cancel_target,
                may_cancel,
            )
            .await?;
            match msg {
                Msg::Row if ex.pre_completes > 0 => {}
                Msg::Row => {
                    ex.rows_seen += 1;
                    if ex.builder.as_ref().is_some_and(BatchBuilder::is_full) {
                        ex.flush_batch();
                        return Ok(Filled::Batch);
                    }
                }
                Msg::Notice(n) => {
                    ex.flush_batch();
                    if ex.push_notice(&n) {
                        // Enough notices for one chunk: deliver them before reading more.
                        return Ok(Filled::Batch);
                    }
                }
                Msg::Error(e) => {
                    if ex.error.is_none() {
                        ex.error = Some(e);
                    }
                    ex.awaiting_ready = true;
                    ex.fetch = Fetch::Finished;
                }
                Msg::Other(m) => match m {
                    backend::Message::CommandComplete(body) => {
                        if ex.pre_completes > 0 {
                            ex.pre_completes -= 1;
                        } else {
                            ex.tag = body.tag().ok().map(str::to_owned);
                            ex.fetch = Fetch::Finished;
                            ex.awaiting_ready = true;
                        }
                    }
                    backend::Message::EmptyQueryResponse => {
                        ex.fetch = Fetch::Finished;
                        ex.awaiting_ready = true;
                    }
                    backend::Message::PortalSuspended => {
                        ex.awaiting_ready = true;
                    }
                    backend::Message::CopyInResponse(_) => {
                        let out = self.conn.out();
                        frontend::copy_fail("COPY FROM STDIN is not supported by NineSQL", out)
                            .map_err(|e| DriverError::Protocol(e.to_string()))?;
                        frontend::sync(out);
                        self.conn.flush().await?;
                    }
                    backend::Message::CopyOutResponse(_) => ex.copy_out = true,
                    backend::Message::ReadyForQuery(_) => {
                        self.sync_status();
                        ex.awaiting_ready = false;
                        if ex.fetch == Fetch::Finished {
                            ex.flush_batch();
                            return Ok(Filled::Finished);
                        }
                        // Portal suspended: check the access mode between batches.
                        ex.fetch = Fetch::Suspended;
                        if self.shared.read_only.load(Ordering::SeqCst)
                            && self.shared.major_version.is_some_and(|v| v >= 14)
                            && self.conn.server.get("default_transaction_read_only") != Some("on")
                        {
                            return Ok(Filled::Finished);
                        }
                        if ex.builder.as_ref().is_some_and(|b| b.rows() > 0) {
                            ex.flush_batch();
                            return Ok(Filled::Batch);
                        }
                    }
                    _ => {}
                },
            }
        }
    }

    /// Reads the rest of an `Execute` response, dropping rows, up to `ReadyForQuery`.
    async fn drain(&mut self, ex: &mut Exec) -> DriverResult<()> {
        while ex.fetch == Fetch::InResponse || ex.awaiting_ready {
            let mut none = None;
            let msg = Self::read_watched(
                &mut self.conn,
                RowSink::Discard,
                ex.watch(),
                &mut none,
                &self.cancel_target,
                false,
            )
            .await?;
            match msg {
                Msg::Error(e) => {
                    if ex.error.is_none() {
                        ex.error = Some(e);
                    }
                    ex.fetch = Fetch::Finished;
                    ex.awaiting_ready = true;
                }
                Msg::Other(
                    backend::Message::CommandComplete(_) | backend::Message::EmptyQueryResponse,
                ) => {
                    if ex.pre_completes > 0 {
                        ex.pre_completes -= 1;
                    } else {
                        ex.fetch = Fetch::Finished;
                        ex.awaiting_ready = true;
                    }
                }
                Msg::Other(backend::Message::PortalSuspended) => ex.awaiting_ready = true,
                Msg::Other(backend::Message::ReadyForQuery(_)) => {
                    self.sync_status();
                    ex.awaiting_ready = false;
                    if ex.fetch == Fetch::InResponse {
                        ex.fetch = Fetch::Suspended;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// The consumer left (`cancelled = false`) or cancelled while no round trip ran: close the
    /// portal without fetching more rows (idle-cursor rule), end the driver's transaction, and
    /// confirm the end.
    async fn abandon(&mut self, ex: &mut Exec, cancelled: bool) -> DriverResult<()> {
        if ex.fetch == Fetch::InResponse
            && !cancelled
            && (ex.class == StatementClass::Read || ex.rows_seen == 0)
        {
            ex.self_cancelled = true;
            // Awaited before the end is confirmed (M6), see `read_watched`.
            let _ = self.cancel_target.send().await;
        }
        ex.deadline = Some(Instant::now() + DROP_GRACE);
        if ex.fetch == Fetch::InResponse {
            self.drain(ex).await?;
        }
        if cancelled {
            ex.cancel.request();
        }
        let error = ex.error.take();
        self.end(ex, None, error).await
    }

    /// Ends the statement: closes an open portal, commits or rolls back the driver's implicit
    /// transaction, checks the session, confirms the end and sends the final chunk.
    async fn end(
        &mut self,
        ex: &mut Exec,
        reply: Option<Demand>,
        error: Option<PgError>,
    ) -> DriverResult<()> {
        ex.flush_batch();
        if ex.notices_dropped > 0 {
            ex.items.push(Ok(ResultItem::Notice(Notice::new(
                NoticeSeverity::Warning,
                format!(
                    "{} more server notices were not shown (too many for one result)",
                    ex.notices_dropped
                ),
            ))));
        }
        let cancelled = ex.cancelled();
        let portal_open = ex.fetch == Fetch::Suspended;
        let mut end_error = None;
        if ex.implicit_txn && self.conn.txn_status != b'I' {
            // COMMIT only when the portal ran to completion (its CommandComplete was seen): a
            // dropped, cancelled or failed statement is rolled back, so a read-class statement
            // with side effects is never partially committed.
            let commit =
                error.is_none() && !cancelled && ex.tag.is_some() && self.conn.txn_status == b'T';
            let out = self.conn.out();
            if portal_open {
                frontend::close(b'P', "", out).map_err(|e| DriverError::Protocol(e.to_string()))?;
            }
            push_fixed(out, if commit { "COMMIT" } else { "ROLLBACK" })?;
            frontend::sync(out);
            end_error = self.fixed_round_trip(ex).await?;
        } else if portal_open && self.conn.txn_status == b'T' {
            let out = self.conn.out();
            frontend::close(b'P', "", out).map_err(|e| DriverError::Protocol(e.to_string()))?;
            frontend::sync(out);
            end_error = self.fixed_round_trip(ex).await?;
        }
        let error = error.or(end_error);
        let mut terminal = match &error {
            Some(e) => Some(statement_error(e, cancelled)),
            None if cancelled => Some(DriverError::Cancelled),
            None => None,
        };
        if terminal.is_none() {
            if ex.copy_out {
                terminal = Some(DriverError::Query {
                    code: None,
                    message: "COPY … TO STDOUT is not supported by NineSQL: its output was \
                              discarded; use the export feature instead"
                        .into(),
                    position: None,
                });
            } else if ex.builder.is_none() {
                let count = ex.tag.as_deref().and_then(tag_count);
                if ex.tag.is_some() {
                    ex.items.push(Ok(ResultItem::UpdateCount(count)));
                }
            } else if let Some(tag) = ex.tag.as_deref()
                && !tag.starts_with("SELECT")
                && !tag.starts_with("FETCH")
            {
                // DML with RETURNING: the rows, then the count.
                ex.items.push(Ok(ResultItem::UpdateCount(tag_count(tag))));
            }
        }
        if let Some(DriverError::Closed(_)) = &terminal {
            self.close_session("the server closed the connection");
        }
        // The access mode and the client encoding are checked after every statement.
        if !self.shared.closed.load(Ordering::SeqCst)
            && let Err(e) = self.verify_session().await
        {
            terminal = Some(e);
        }
        self.finish(ex, reply, terminal);
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Filled {
    Batch,
    Finished,
}

/// A driver statement whose rows passed the internal byte cap: non-fatal (the rest of the
/// result was read and dropped, the session and its transaction stay).
pub(crate) fn internal_overflow() -> DriverError {
    DriverError::Query {
        code: None,
        message: "the listing is larger than the driver's 32 MiB limit; use a smaller page or a \
                  filter"
            .into(),
        position: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_and_params() {
        assert_eq!(tag_count("INSERT 0 5"), Some(5));
        assert_eq!(tag_count("UPDATE 3"), Some(3));
        assert_eq!(tag_count("SELECT 10"), Some(10));
        assert_eq!(tag_count("CREATE TABLE"), None);
        let mut b = BytesMut::new();
        encode_param(&Param::Bytes(vec![0xde, 0xad]), &mut b).unwrap();
        assert_eq!(&b[..], b"\\xdead");
        let mut b = BytesMut::new();
        encode_param(&Param::Date(0), &mut b).unwrap();
        assert_eq!(&b[..], b"1970-01-01");
        let mut b = BytesMut::new();
        encode_param(
            &Param::Timestamp {
                micros: -1,
                utc: true,
            },
            &mut b,
        )
        .unwrap();
        assert_eq!(&b[..], b"1969-12-31 23:59:59.999999+00");
        let mut b = BytesMut::new();
        encode_param(&Param::Float(f64::NEG_INFINITY), &mut b).unwrap();
        assert_eq!(&b[..], b"-Infinity");
        assert!(matches!(
            encode_param(&Param::Null, &mut BytesMut::new()).unwrap(),
            IsNull::Yes
        ));
        assert_eq!(civil_from_days(19_000), (2022, 1, 8));
    }

    #[test]
    fn errors_map() {
        let e = |code: &str, sev: &str| PgError {
            severity: sev.into(),
            code: code.into(),
            message: "m".into(),
            hint: None,
            position: Some(3),
        };
        assert!(matches!(
            statement_error(&e("25006", "ERROR"), false),
            DriverError::ReadOnly(_)
        ));
        assert!(matches!(
            statement_error(&e("57014", "ERROR"), true),
            DriverError::Cancelled
        ));
        assert!(matches!(
            statement_error(&e("57014", "ERROR"), false),
            DriverError::Query { .. }
        ));
        assert!(matches!(
            statement_error(&e("25P03", "FATAL"), false),
            DriverError::Closed(_)
        ));
    }
}
