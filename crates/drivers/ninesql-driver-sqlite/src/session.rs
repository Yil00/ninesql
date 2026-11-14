//! [`Session`] and [`Metadata`] of the SQLite driver: thin fronts that check the contract (one
//! statement, classification, read-only refusal, busy state) and send commands to the
//! connection thread.

use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, PoisonError};

use ninesql_driver_api::{
    CancelHandle, Capabilities, Dialect, DriverError, DriverResult, ExecOptions, ExecutionTracker,
    Feature, IsolationLevel, Metadata, ObjectInfo, ObjectKind, ObjectPath, Page, PageRequest,
    QueryLanguage, RawResultStream, ResultItem, ResultStream, Session, Statement,
    ensure_single_statement,
};
use rusqlite::InterruptHandle;
use tokio::sync::oneshot;

use crate::dialect::SqliteDialect;
use crate::worker::{
    Chunk, Command, ExecRequest, ExecState, INTERNAL_TIMEOUT, Op, OpOut, Shared, params_supported,
};

/// Handles shared by the session and its metadata.
#[derive(Clone)]
pub(crate) struct Link {
    pub(crate) tx: Sender<Command>,
    pub(crate) shared: Arc<Shared>,
    pub(crate) tracker: ExecutionTracker,
}

impl Link {
    /// Runs a driver operation as one execution of the session (busy while a result streams).
    pub(crate) async fn op(&self, op: Op) -> DriverResult<OpOut> {
        if self.shared.closed.load(Ordering::SeqCst) {
            return Err(self.shared.closed_error());
        }
        let guard = self.tracker.begin()?;
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Command::Op { op, guard, reply })
            .map_err(|_| self.shared.closed_error())?;
        // The thread bounds each driver statement; this also bounds a thread that never
        // answers (a busy handler or a stuck file system). Past it the session is closed.
        match tokio::time::timeout(INTERNAL_TIMEOUT * 2, rx).await {
            Ok(r) => r.map_err(|_| self.shared.closed_error())?,
            Err(_) => {
                self.shared.mark_closed("a driver statement timed out");
                Err(self.shared.closed_error())
            }
        }
    }
}

/// Server-side cancel of one execution: `sqlite3_interrupt`, plus a flag the connection thread
/// reads between rows and while a result is paused.
struct SqliteCancel {
    interrupt: Arc<InterruptHandle>,
    exec: Arc<ExecState>,
}

#[async_trait::async_trait]
impl CancelHandle for SqliteCancel {
    async fn cancel(&self) -> DriverResult<()> {
        self.exec.cancelled.store(true, Ordering::SeqCst);
        self.interrupt.interrupt();
        Ok(())
    }

    fn confirms_end(&self) -> bool {
        true
    }
}

/// A SQLite session.
pub struct SqliteSession {
    pub(crate) link: Link,
    pub(crate) capabilities: Capabilities,
    pub(crate) interrupt: Arc<InterruptHandle>,
    pub(crate) production: bool,
    /// State of the current execution, shared with `Session::cancel_handle`.
    pub(crate) current_exec: Mutex<Arc<ExecState>>,
}

impl std::fmt::Debug for SqliteSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteSession")
            .field("server_version", &self.capabilities.server_version)
            .field("read_only", &self.is_read_only())
            .finish_non_exhaustive()
    }
}

impl SqliteSession {
    fn engine_cancel(&self, exec: Arc<ExecState>) -> Arc<dyn CancelHandle> {
        Arc::new(SqliteCancel {
            interrupt: Arc::clone(&self.interrupt),
            exec,
        })
    }

    fn raw_stream(
        demands: Sender<oneshot::Sender<Chunk>>,
        exec: Arc<ExecState>,
        shared: Arc<Shared>,
    ) -> RawResultStream {
        struct State {
            demands: Option<Sender<oneshot::Sender<Chunk>>>,
            queue: VecDeque<DriverResult<ResultItem>>,
            exec: Arc<ExecState>,
            shared: Arc<Shared>,
        }
        impl State {
            /// The thread stopped listening: the execution ended (a paused result closed,
            /// a cancel acknowledged) or the session was lost.
            fn ended(&mut self) -> Option<DriverResult<ResultItem>> {
                self.demands = None;
                if self.exec.ended.load(Ordering::SeqCst) {
                    self.exec
                        .end_error
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .take()
                        .map(Err)
                } else {
                    Some(Err(self.shared.closed_error()))
                }
            }
        }
        let state = State {
            demands: Some(demands),
            queue: VecDeque::new(),
            exec,
            shared,
        };
        Box::pin(futures_util::stream::unfold(state, |mut st| async move {
            loop {
                if let Some(item) = st.queue.pop_front() {
                    return Some((item, st));
                }
                let tx = st.demands.as_ref()?;
                let (reply, rx) = oneshot::channel::<Chunk>();
                if tx.send(reply).is_err() {
                    return st.ended().map(|i| (i, st));
                }
                match rx.await {
                    Ok(chunk) => {
                        if chunk.done {
                            st.demands = None;
                        }
                        st.queue.extend(chunk.items);
                    }
                    Err(_) => return st.ended().map(|i| (i, st)),
                }
            }
        }))
    }

    async fn sql(&self, sql: String) -> DriverResult<()> {
        self.link.op(Op::Sql(sql)).await.map(drop)
    }
}

#[async_trait::async_trait]
impl Session for SqliteSession {
    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    fn server_version(&self) -> Option<&str> {
        self.capabilities.server_version.as_deref()
    }

    fn dialect(&self) -> Arc<dyn Dialect> {
        Arc::new(SqliteDialect)
    }

    fn metadata(&self) -> Arc<dyn Metadata> {
        Arc::new(SqliteMetadata {
            link: self.link.clone(),
        })
    }

    async fn execute(
        &mut self,
        statement: Statement,
        options: ExecOptions,
    ) -> DriverResult<ResultStream> {
        if statement.language != QueryLanguage::Sql {
            return Err(DriverError::Unsupported(Feature::Language(
                statement.language,
            )));
        }
        if !params_supported(&statement.params) {
            return Err(DriverError::Unsupported(Feature::Parameters));
        }
        let shared = &self.link.shared;
        if shared.closed.load(Ordering::SeqCst) {
            return Err(shared.closed_error());
        }
        if self.link.tracker.is_busy() {
            return Err(DriverError::Busy);
        }
        if statement.text.contains('\0') {
            return Err(DriverError::InvalidParams(
                "the statement holds a NUL character".into(),
            ));
        }
        // One statement per call (SQLite's own `prepare` also refuses a second one).
        ensure_single_statement(&SqliteDialect, &statement.text)?;
        let class = SqliteDialect.classify(&statement.text);
        if self.is_read_only() && class.is_write() {
            return Err(DriverError::ReadOnly(
                "statement refused: the session is read-only".into(),
            ));
        }
        if !self.production {
            tracing::trace!(statement = %statement.text, "sqlite execute");
        }
        let guard = self.link.tracker.begin()?;
        let id = guard.id();
        let exec = Arc::new(ExecState::default());
        *self
            .current_exec
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Arc::clone(&exec);
        let canceller = self
            .link
            .tracker
            .cancel_handle(id, self.engine_cancel(Arc::clone(&exec)));
        let (demand_tx, demand_rx) = std::sync::mpsc::channel();
        let request = ExecRequest {
            text: statement.text,
            params: statement.params,
            budget: options.budget,
            class,
            guard,
            exec: Arc::clone(&exec),
            demands: demand_rx,
        };
        self.link
            .tx
            .send(Command::Execute(Box::new(request)))
            .map_err(|_| shared.closed_error())?;
        let raw = Self::raw_stream(demand_tx, exec, Arc::clone(shared));
        Ok(ResultStream::new(raw, &options, canceller).with_execution(id))
    }

    fn cancel_handle(&self) -> Arc<dyn CancelHandle> {
        // Scoped to the execution running now (a no-op when idle or once it ended).
        let exec = Arc::clone(
            &self
                .current_exec
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        self.link
            .tracker
            .current_cancel_handle(self.engine_cancel(exec))
    }

    fn auto_commit(&self) -> bool {
        self.link.shared.auto_commit.load(Ordering::SeqCst)
    }

    fn in_transaction(&self) -> bool {
        self.link.shared.in_tx.load(Ordering::SeqCst)
    }

    async fn set_auto_commit(&mut self, auto_commit: bool) -> DriverResult<()> {
        self.link.op(Op::SetAutoCommit(auto_commit)).await.map(drop)
    }

    async fn begin(&mut self) -> DriverResult<()> {
        self.sql("BEGIN".to_owned()).await
    }

    async fn commit(&mut self) -> DriverResult<()> {
        self.link.op(Op::Commit).await.map(drop)
    }

    async fn rollback(&mut self) -> DriverResult<()> {
        self.link.op(Op::Rollback).await.map(drop)
    }

    async fn savepoint(&mut self, name: &str) -> DriverResult<()> {
        self.sql(format!(
            "SAVEPOINT {}",
            SqliteDialect.quote_identifier(name)
        ))
        .await
    }

    async fn rollback_to_savepoint(&mut self, name: &str) -> DriverResult<()> {
        self.sql(format!(
            "ROLLBACK TO SAVEPOINT {}",
            SqliteDialect.quote_identifier(name)
        ))
        .await
    }

    async fn set_isolation_level(&mut self, level: IsolationLevel) -> DriverResult<()> {
        // SQLite transactions are serializable; no other level can be chosen.
        if level == IsolationLevel::Serializable {
            Ok(())
        } else {
            Err(DriverError::Unsupported(Feature::Transactions))
        }
    }

    fn is_read_only(&self) -> bool {
        self.link.shared.read_only
    }

    async fn set_read_only(&mut self, read_only: bool) -> DriverResult<()> {
        if read_only == self.is_read_only() {
            Ok(())
        } else {
            // The mechanism is the open mode of the file: reconnect with the new mode.
            Err(DriverError::Unsupported(Feature::EngineSideReadOnly))
        }
    }

    async fn ping(&mut self) -> DriverResult<()> {
        self.link.op(Op::Ping).await.map(drop)
    }

    async fn close(self: Box<Self>) -> DriverResult<()> {
        let shared = &self.link.shared;
        // A result left paused gives up; a running statement stops.
        shared.closing.store(true, Ordering::SeqCst);
        self.interrupt.interrupt();
        let (reply, rx) = oneshot::channel();
        if self.link.tx.send(Command::Close { reply }).is_ok() {
            let _ = tokio::time::timeout(INTERNAL_TIMEOUT, rx).await;
        }
        shared.mark_closed("session closed");
        Ok(())
    }
}

/// Navigator metadata, served by the session's connection thread: a call made while a result
/// streams fails with [`DriverError::Busy`]. Names, filters and cursors are bound parameters;
/// a schema name reaches the SQL only as a quoted identifier, after it was found in
/// `pragma_database_list`.
pub struct SqliteMetadata {
    link: Link,
}

impl std::fmt::Debug for SqliteMetadata {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteMetadata").finish_non_exhaustive()
    }
}

#[async_trait::async_trait]
impl Metadata for SqliteMetadata {
    fn child_kinds(&self, parent: Option<ObjectKind>) -> Vec<ObjectKind> {
        match parent {
            None => vec![ObjectKind::Schema],
            Some(ObjectKind::Schema) => vec![ObjectKind::Table, ObjectKind::View],
            Some(ObjectKind::Table | ObjectKind::View) => vec![ObjectKind::Column],
            Some(_) => Vec::new(),
        }
    }

    async fn list(
        &self,
        parent: &ObjectPath,
        kind: ObjectKind,
        page: PageRequest,
    ) -> DriverResult<Page<ObjectInfo>> {
        match self
            .link
            .op(Op::List {
                parent: parent.clone(),
                kind,
                page,
            })
            .await?
        {
            OpOut::Page(p) => Ok(p),
            _ => Err(DriverError::Protocol("unexpected navigator answer".into())),
        }
    }

    async fn ddl(&self, object: &ObjectPath) -> DriverResult<String> {
        match self.link.op(Op::Ddl(object.clone())).await? {
            OpOut::Ddl(s) => Ok(s),
            _ => Err(DriverError::Protocol("unexpected navigator answer".into())),
        }
    }
}
