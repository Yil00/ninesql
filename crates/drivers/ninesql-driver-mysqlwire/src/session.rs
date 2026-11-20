//! [`Session`] of the mysqlwire driver: thin fronts that check the contract
//! (one statement, classification, read-only refusal, busy state) and send commands to the
//! connection task.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use ninesql_driver_api::{
    CancelHandle, Capabilities, Dialect, DriverError, DriverResult, EngineId, ExecOptions,
    ExecutionTracker, Feature, IsolationLevel, Metadata, QueryLanguage, ResultItem, ResultStream,
    Session, Statement, StatementClass, ensure_single_statement,
};
use tokio::sync::{mpsc, oneshot};

use crate::actor::{Chunk, Command, Demand, ExecRequest, Rows, Shared};
use crate::cancel::{Control, ExecCancel, MyCancel};
use crate::dialect::{MyDialect, params_supported};
use crate::metadata::MyMetadata;

/// Handles shared by the session and its metadata.
#[derive(Clone)]
pub(crate) struct Link {
    pub(crate) tx: mpsc::Sender<Command>,
    pub(crate) shared: Arc<Shared>,
    pub(crate) tracker: ExecutionTracker,
}

impl Link {
    /// Runs a driver statement (text results), as one execution of the session.
    pub(crate) async fn internal(&self, sql: &str, verify: bool) -> DriverResult<Rows> {
        self.internal_limited(sql, verify, None, None).await
    }

    /// A navigator read ([`MyMetadata`]): like
    /// [`internal_limited`](Self::internal_limited) with the control connection, and in
    /// manual-commit mode the transaction the read started by itself is rolled back
    /// afterwards; a transaction the user opened stays open, untouched.
    pub(crate) async fn navigator(
        &self,
        sql: &str,
        value_limit: Option<u64>,
        control: &Arc<Control>,
    ) -> DriverResult<Rows> {
        self.send_internal(sql, false, value_limit, Some(control), true)
            .await
    }

    /// [`internal`](Self::internal) with a value limit (`Command::Internal::value_limit`) and,
    /// for navigator calls, the control connection: if the caller drops the future before the
    /// answer, the statement is stopped with `KILL QUERY` (scoped to this execution).
    pub(crate) async fn internal_limited(
        &self,
        sql: &str,
        verify: bool,
        value_limit: Option<u64>,
        cancel_on_drop: Option<&Arc<Control>>,
    ) -> DriverResult<Rows> {
        self.send_internal(sql, verify, value_limit, cancel_on_drop, false)
            .await
    }

    async fn send_internal(
        &self,
        sql: &str,
        verify: bool,
        value_limit: Option<u64>,
        cancel_on_drop: Option<&Arc<Control>>,
        navigator: bool,
    ) -> DriverResult<Rows> {
        if self.shared.closed.load(Ordering::SeqCst) {
            return Err(self.shared.closed_error());
        }
        let guard = self.tracker.begin()?;
        let mut on_drop = cancel_on_drop.map(|control| CancelOnDrop {
            tracker: self.tracker.clone(),
            id: guard.id(),
            control: Arc::clone(control),
            armed: true,
        });
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Command::Internal {
                sql: sql.to_owned(),
                value_limit,
                verify,
                navigator,
                guard,
                reply,
            })
            .await
            .map_err(|_| self.shared.closed_error())?;
        // The connection task bounds the statement itself; this also bounds a queue that
        // never drains (a stuck task). Past it the session is closed.
        let result = match tokio::time::timeout(crate::actor::INTERNAL_TIMEOUT * 2, rx).await {
            Ok(r) => r.map_err(|_| self.shared.closed_error()),
            Err(_) => {
                self.shared.mark_closed("driver statement timed out");
                Err(self.shared.closed_error())
            }
        };
        // Answered: nothing left to cancel.
        if let Some(d) = on_drop.as_mut() {
            d.armed = false;
        }
        result?
    }
}

/// Stops execution `id` with `KILL QUERY` when dropped (a navigator call abandoned by its
/// caller, for example by the core's timeout). Scoped by the tracker: a no-op once `id` ended.
struct CancelOnDrop {
    tracker: ExecutionTracker,
    id: ninesql_driver_api::ExecutionId,
    control: Arc<Control>,
    armed: bool,
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let handle = self.tracker.cancel_handle(
            self.id,
            Arc::new(MyCancel {
                control: Arc::clone(&self.control),
                exec: Arc::new(ExecCancel::default()),
            }),
        );
        rt.spawn(async move {
            let _ = handle.cancel().await;
        });
    }
}

/// A MySQL or MariaDB session.
pub struct MySession {
    pub(crate) link: Link,
    pub(crate) capabilities: Capabilities,
    pub(crate) dialect: Arc<MyDialect>,
    pub(crate) control: Arc<Control>,
    /// Statement text may be logged at `trace` level (`ConnectParams::may_log_statements`:
    /// the connection type allows it and the connection is not production, ADR-0007 §6), and
    /// only a text that holds no credential (`may_log_statement_text`, I1).
    pub(crate) log_statements: bool,
    /// Cancel flag of the current execution, shared with `Session::cancel_handle`.
    pub(crate) current_exec: std::sync::Mutex<Arc<ExecCancel>>,
}

impl std::fmt::Debug for MySession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MySession")
            .field("engine", &self.capabilities.engine)
            .field("server_version", &self.capabilities.server_version)
            .field("read_only", &self.is_read_only())
            .finish_non_exhaustive()
    }
}

impl MySession {
    /// Sends a statement that passed the classify gate of `Session::execute` (this crate's
    /// tests also use it to skip the gate). Private: `class` is the dialect's classification.
    pub(crate) async fn dispatch(
        &mut self,
        statement: Statement,
        options: ExecOptions,
        class: StatementClass,
    ) -> DriverResult<ResultStream> {
        let shared = &self.link.shared;
        let text = self.dialect.bind(&statement.text, &statement.params)?;
        // Never a text that may hold a credential, init SQL included (I1).
        if ninesql_driver_api::may_log_statement_text(self.log_statements, &statement.text) {
            tracing::trace!(statement = %statement.text, "mysqlwire execute");
        }
        let guard = self.link.tracker.begin()?;
        let id = guard.id();
        let exec = Arc::new(ExecCancel::default());
        *self
            .current_exec
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Arc::clone(&exec);
        let canceller = self
            .link
            .tracker
            .cancel_handle(id, self.engine_cancel(Arc::clone(&exec)));
        let (demand_tx, demand_rx) = mpsc::channel(1);
        let request = ExecRequest {
            text,
            budget: options.budget,
            class,
            guard,
            cancel: exec,
            demands: demand_rx,
        };
        self.link
            .tx
            .send(Command::Execute(Box::new(request)))
            .await
            .map_err(|_| shared.closed_error())?;
        let raw = Self::raw_stream(demand_tx, Arc::clone(shared));
        Ok(ResultStream::new(raw, &options, canceller).with_execution(id))
    }

    fn engine_cancel(&self, exec: Arc<ExecCancel>) -> Arc<dyn CancelHandle> {
        Arc::new(MyCancel {
            control: Arc::clone(&self.control),
            exec,
        })
    }

    pub(crate) async fn run(&self, sql: &str) -> DriverResult<Rows> {
        self.link.internal(sql, false).await
    }

    /// Applies (or lifts) the engine-side read-only mode on the session and the control
    /// connection, and verifies it on the session by reading it back.
    pub(crate) async fn apply_read_only(&mut self, read_only: bool) -> DriverResult<()> {
        if self.link.shared.tidb {
            return self.apply_tidb_read_only(read_only).await;
        }
        let sql = if read_only {
            "SET SESSION TRANSACTION READ ONLY"
        } else {
            "SET SESSION TRANSACTION READ WRITE"
        };
        // The control connection first (it only runs `KILL QUERY`), bounded: if it does not
        // answer, the operation fails and the session keeps its previous mode.
        if let Err(e) = self.control.setup(sql).await {
            return Err(if read_only {
                DriverError::ReadOnly(format!("cannot enable read-only mode: {e}"))
            } else {
                e
            });
        }
        // The verification inside `internal` must not see the old flag mid-change.
        let was = self.link.shared.read_only.swap(false, Ordering::SeqCst);
        if let Err(e) = self.run(sql).await {
            self.link.shared.read_only.store(was, Ordering::SeqCst);
            return Err(if read_only {
                DriverError::ReadOnly(format!("cannot enable read-only mode: {e}"))
            } else {
                e
            });
        }
        let check = format!("SELECT {}", self.link.shared.ro_expr);
        let rows = match self.run(&check).await {
            Ok(r) => r,
            Err(e) => {
                self.link.shared.read_only.store(was, Ordering::SeqCst);
                return Err(e);
            }
        };
        let value = rows
            .first()
            .and_then(|r| r.first())
            .and_then(Option::as_deref);
        let expected = if read_only { "1" } else { "0" };
        if value != Some(expected) {
            self.link.shared.read_only.store(was, Ordering::SeqCst);
            return Err(DriverError::ReadOnly(
                "the server did not confirm the read-only setting".into(),
            ));
        }
        self.link
            .shared
            .read_only
            .store(read_only, Ordering::SeqCst);
        Ok(())
    }

    /// TiDB has no session read-only mode: `SET SESSION TRANSACTION READ ONLY` is a no-op
    /// that TiDB refuses by default (error 1235). A session is read-only only when the whole
    /// instance refuses writes (`tidb_super_read_only` / `tidb_restricted_read_only`) and the
    /// refusal applies to this account: no `RESTRICTED_REPLICA_WRITER_ADMIN` / `SUPER` /
    /// `ALL PRIVILEGES ON *.*` in `SHOW GRANTS`, and a write probe refused with 1836
    /// ([`crate::tidb`]). The instance's mode and the active roles are read back after every
    /// statement; anything else is refused, never faked (I3, I8). Leaving read-only mode only
    /// clears the driver's flag: the instance stays as it is.
    async fn apply_tidb_read_only(&mut self, read_only: bool) -> DriverResult<()> {
        let shared = Arc::clone(&self.link.shared);
        if !read_only {
            shared.read_only.store(false, Ordering::SeqCst);
            *shared
                .tidb_role
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
            return Ok(());
        }
        let check = format!("SELECT {}, CURRENT_ROLE()", shared.ro_expr);
        let row = self
            .run(&check)
            .await?
            .into_iter()
            .next()
            .unwrap_or_default();
        if row.first().cloned().flatten().as_deref() != Some("1") {
            return Err(DriverError::ReadOnly(
                "TiDB has no session read-only mode (SET TRANSACTION READ ONLY is a no-op \
                 there), so a read-only session is refused unless the TiDB instance itself is \
                 read-only (tidb_super_read_only or tidb_restricted_read_only); use an account \
                 with SELECT privileges only, on a connection that is not read-only"
                    .into(),
            ));
        }
        let Some(role) = row.get(1).cloned().flatten() else {
            return Err(DriverError::ReadOnly(
                "the TiDB session's active roles could not be read; read-only mode refused".into(),
            ));
        };
        // The account's privileges, active roles expanded; a failed read is a refusal.
        let grants = self
            .link
            .internal_limited("SHOW GRANTS", false, Some(64 * 1024), None)
            .await
            .map_err(|e| {
                tracing::warn!(
                    driver_error_kind = crate::logkind::error_kind(&e),
                    code = crate::logkind::error_code(&e),
                    "SHOW GRANTS failed"
                );
                DriverError::ReadOnly(
                    "the account's privileges could not be read (SHOW GRANTS); read-only mode \
                     refused"
                        .into(),
                )
            })?;
        if let Some(reason) = crate::tidb::grants_refusal(&grants) {
            return Err(DriverError::ReadOnly(format!(
                "{reason}, which lets it write while the TiDB instance is read-only, so a \
                 read-only session is refused; use an account with SELECT privileges only"
            )));
        }
        // The probe changes nothing whatever the account; only error 1836 proves the gate.
        let probe = self.run(crate::tidb::WRITE_PROBE).await;
        let gated = crate::tidb::probe_gated(&probe);
        // A manual-commit session may count the failed statement as the start of a
        // transaction: nothing was written, end it (no user transaction is open here:
        // `set_read_only` refuses inside one, and connect has run no user statement yet).
        if shared.in_transaction() {
            self.run("ROLLBACK").await?;
        }
        if !gated {
            return Err(DriverError::ReadOnly(
                "the TiDB instance is read-only, but a write probe was not refused with \
                 \"read-only mode\" for this account (it may hold a privilege that exempts it), \
                 so a read-only session is refused; use an account with SELECT privileges only"
                    .into(),
            ));
        }
        *shared
            .tidb_role
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(role);
        shared.read_only.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn raw_stream(
        demands: mpsc::Sender<Demand>,
        shared: Arc<Shared>,
    ) -> ninesql_driver_api::RawResultStream {
        struct State {
            demands: Option<mpsc::Sender<Demand>>,
            queue: VecDeque<DriverResult<ResultItem>>,
            shared: Arc<Shared>,
        }
        let state = State {
            demands: Some(demands),
            queue: VecDeque::new(),
            shared,
        };
        Box::pin(futures_util::stream::unfold(state, |mut st| async move {
            loop {
                if let Some(item) = st.queue.pop_front() {
                    return Some((item, st));
                }
                let tx = st.demands.as_ref()?;
                let (reply, rx) = oneshot::channel::<Chunk>();
                if tx.send(reply).await.is_err() {
                    st.demands = None;
                    return Some((Err(st.shared.closed_error()), st));
                }
                match rx.await {
                    Ok(chunk) => {
                        if chunk.done {
                            st.demands = None;
                        }
                        st.queue.extend(chunk.items);
                    }
                    Err(_) => {
                        st.demands = None;
                        return Some((Err(st.shared.closed_error()), st));
                    }
                }
            }
        }))
    }
}

#[async_trait::async_trait]
impl Session for MySession {
    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    fn server_version(&self) -> Option<&str> {
        self.capabilities.server_version.as_deref()
    }

    fn dialect(&self) -> Arc<dyn Dialect> {
        Arc::clone(&self.dialect) as Arc<dyn Dialect>
    }

    fn metadata(&self) -> Arc<dyn Metadata> {
        Arc::new(MyMetadata {
            link: self.link.clone(),
            control: Arc::clone(&self.control),
            mariadb: self.capabilities.engine == EngineId::MARIADB,
            tidb: self.capabilities.engine == EngineId::TIDB,
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
        // A NUL byte ends the text for some lexers and not for others: refused, as in sqlite
        // (security review L5).
        if statement.text.contains('\0') {
            return Err(DriverError::InvalidParams(
                "the statement holds a NUL character".into(),
            ));
        }
        // One statement per protocol call, with the session's current lexical rules.
        ensure_single_statement(self.dialect.as_ref(), &statement.text)?;
        let class = self.dialect.classify(&statement.text);
        if self.is_read_only() && class.is_write() {
            return Err(DriverError::ReadOnly(
                "statement refused: the session is read-only".into(),
            ));
        }
        self.dispatch(statement, options, class).await
    }

    fn cancel_handle(&self) -> Arc<dyn CancelHandle> {
        // Scoped to the execution running now (a no-op when idle or once it ended).
        let exec = Arc::clone(
            &self
                .current_exec
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        self.link
            .tracker
            .current_cancel_handle(self.engine_cancel(exec))
    }

    fn auto_commit(&self) -> bool {
        self.link.shared.auto_commit()
    }

    fn in_transaction(&self) -> bool {
        self.link.shared.in_transaction()
    }

    async fn set_auto_commit(&mut self, auto_commit: bool) -> DriverResult<()> {
        // `autocommit = 1` commits an open transaction (JDBC semantics).
        let sql = if auto_commit {
            "SET SESSION autocommit = 1"
        } else {
            "SET SESSION autocommit = 0"
        };
        self.run(sql).await.map(drop)
    }

    async fn begin(&mut self) -> DriverResult<()> {
        self.run("START TRANSACTION").await.map(drop)
    }

    async fn commit(&mut self) -> DriverResult<()> {
        self.run("COMMIT").await.map(drop)
    }

    async fn rollback(&mut self) -> DriverResult<()> {
        self.run("ROLLBACK").await.map(drop)
    }

    async fn savepoint(&mut self, name: &str) -> DriverResult<()> {
        let sql = format!("SAVEPOINT {}", self.dialect.quote_identifier(name));
        self.run(&sql).await.map(drop)
    }

    async fn rollback_to_savepoint(&mut self, name: &str) -> DriverResult<()> {
        let sql = format!(
            "ROLLBACK TO SAVEPOINT {}",
            self.dialect.quote_identifier(name)
        );
        self.run(&sql).await.map(drop)
    }

    async fn set_isolation_level(&mut self, level: IsolationLevel) -> DriverResult<()> {
        // Only the declared levels (TiDB refuses SERIALIZABLE and READ UNCOMMITTED).
        if !self
            .capabilities
            .transactions
            .isolation_levels
            .contains(&level)
        {
            return Err(DriverError::Unsupported(Feature::Transactions));
        }
        let level = match level {
            IsolationLevel::ReadUncommitted => "READ UNCOMMITTED",
            IsolationLevel::ReadCommitted => "READ COMMITTED",
            IsolationLevel::RepeatableRead => "REPEATABLE READ",
            IsolationLevel::Serializable => "SERIALIZABLE",
            IsolationLevel::Snapshot => {
                return Err(DriverError::Unsupported(Feature::Transactions));
            }
        };
        let sql = format!("SET SESSION TRANSACTION ISOLATION LEVEL {level}");
        self.run(&sql).await.map(drop)
    }

    fn is_read_only(&self) -> bool {
        self.link.shared.read_only.load(Ordering::SeqCst)
    }

    async fn set_read_only(&mut self, read_only: bool) -> DriverResult<()> {
        if read_only == self.is_read_only() {
            return Ok(());
        }
        if self.in_transaction() {
            return Err(DriverError::ReadOnly(
                "commit or roll back the current transaction before changing read-only mode".into(),
            ));
        }
        self.apply_read_only(read_only).await
    }

    async fn ping(&mut self) -> DriverResult<()> {
        let shared = &self.link.shared;
        if shared.closed.load(Ordering::SeqCst) {
            return Err(shared.closed_error());
        }
        let guard = self.link.tracker.begin()?;
        let (reply, rx) = oneshot::channel();
        self.link
            .tx
            .send(Command::Ping { guard, reply })
            .await
            .map_err(|_| shared.closed_error())?;
        rx.await.map_err(|_| shared.closed_error())?
    }

    async fn close(self: Box<Self>) -> DriverResult<()> {
        let (reply, rx) = oneshot::channel();
        if self.link.tx.send(Command::Close { reply }).await.is_ok() {
            let _ = rx.await;
        } else {
            self.control.close().await;
        }
        self.link.shared.mark_closed("session closed");
        Ok(())
    }
}

/// Security review follow-up: the driver's `SET SESSION TRANSACTION READ ONLY` re-assert,
/// tested past the classifier. The one-shot `READ WRITE` comma form is sent through `dispatch`
/// with a forged `Session` class, as a classifier gap would let it through; a following write
/// must still fail engine-side. Each engine is skipped when unreachable, unless
/// `NINESQL_REQUIRE_ENGINES` lists it.
#[cfg(test)]
mod read_only_reassert {
    use std::time::Duration;

    use ninesql_driver_api::{
        Auth, ConnectParams, DriverError, Endpoint, EngineId, ExecOptions, Secret, Session,
        Statement, StatementClass, TlsMode,
    };

    use super::MySession;
    use crate::MySqlWireDriver;

    const ONE_SHOT: &str = "SET TRANSACTION ISOLATION LEVEL READ COMMITTED, READ WRITE";
    const WRITE: &str = "SELECT ninesql_reassert_write()";

    fn dev_params(id: &str, engine: EngineId) -> Option<ConnectParams> {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../dev/engines.json");
        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let e = doc["engines"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["id"] == id)
            .unwrap();
        let host = e["host"].as_str().unwrap();
        let port = u16::try_from(e["port"].as_u64().unwrap()).unwrap();
        let addr = format!("{host}:{port}");
        let reachable = addr
            .parse()
            .ok()
            .and_then(|a| std::net::TcpStream::connect_timeout(&a, Duration::from_secs(2)).ok())
            .is_some();
        if !reachable {
            let required = std::env::var("NINESQL_REQUIRE_ENGINES")
                .unwrap_or_default()
                .split(',')
                .any(|s| s.trim() == id);
            assert!(!required, "{id} is required but unreachable at {addr}");
            return None;
        }
        let mut p = ConnectParams::new(engine, Endpoint::tcp(host, port));
        p.auth = Auth::password(
            e["user"].as_str().unwrap(),
            Some(Secret::new(e["password"].as_str().unwrap().to_owned())),
        );
        p.database = Some(e["database"].as_str().unwrap().to_owned());
        p.tls.mode = TlsMode::Disable;
        p.application_name = "ninesql-mysqlwire-reassert".into();
        Some(p)
    }

    async fn drain(
        s: &mut MySession,
        sql: &str,
        class: Option<StatementClass>,
    ) -> Result<(), DriverError> {
        let st = Statement::sql(sql);
        let mut stream = match class {
            // Past the classify gate of `Session::execute`.
            Some(c) => s.dispatch(st, ExecOptions::default(), c).await?,
            None => s.execute(st, ExecOptions::default()).await?,
        };
        while let Some(item) = stream.next().await {
            item?;
        }
        Ok(())
    }

    async fn count(s: &mut MySession) -> String {
        let mut stream = s
            .execute(
                Statement::sql("SELECT CAST(COUNT(*) AS CHAR) FROM ninesql_reassert_written"),
                ExecOptions::default(),
            )
            .await
            .unwrap();
        let mut n = None;
        while let Some(item) = stream.next().await {
            if let ninesql_driver_api::ResultItem::Rows(b) = item.unwrap() {
                use ninesql_driver_api::arrow_array::cast::AsArray;
                n = Some(b.column(0).as_string::<i32>().value(0).to_owned());
            }
        }
        n.unwrap()
    }

    fn assert_refused(r: Result<(), DriverError>, case: &str) {
        assert!(
            matches!(r, Err(DriverError::ReadOnly(_))),
            "{case}: write after a one-shot READ WRITE sent past the classifier: {r:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn one_shot_read_write_past_the_classifier_cannot_write() {
        let driver = MySqlWireDriver::new();
        for (id, engine) in [("mysql", EngineId::MYSQL), ("mariadb", EngineId::MARIADB)] {
            let Some(params) = dev_params(id, engine.clone()) else {
                continue;
            };
            let mut w = driver.open(dev_params(id, engine).unwrap()).await.unwrap();
            for sql in [
                "CREATE TABLE IF NOT EXISTS ninesql_reassert_written (a INT)",
                "DROP FUNCTION IF EXISTS ninesql_reassert_write",
                "CREATE FUNCTION ninesql_reassert_write() RETURNS INT DETERMINISTIC MODIFIES \
                 SQL DATA BEGIN INSERT INTO ninesql_reassert_written VALUES (1); RETURN 1; END",
            ] {
                drain(&mut w, sql, None).await.unwrap();
            }
            Box::new(w).close().await.unwrap();

            let mut p = params;
            p.read_only = true;
            let mut s = driver.open(p).await.unwrap();
            let before = count(&mut s).await;
            // MySQL refuses transaction characteristics inside a transaction (1568): the
            // one-shot is sent between transactions and applies to the next one.
            for mode in ["autocommit", "manual", "explicit"] {
                s.set_auto_commit(mode != "manual").await.unwrap();
                drain(&mut s, ONE_SHOT, Some(StatementClass::Session))
                    .await
                    .unwrap();
                if mode == "explicit" {
                    s.begin().await.unwrap();
                }
                assert_refused(drain(&mut s, WRITE, None).await, &format!("{id} {mode}"));
                if mode != "autocommit" {
                    s.rollback().await.unwrap();
                }
                assert!(s.is_read_only());
            }
            s.set_auto_commit(true).await.unwrap();
            // Savepoints: the one-shot, a new transaction, SAVEPOINT, a write, ROLLBACK TO, a
            // write.
            drain(&mut s, ONE_SHOT, Some(StatementClass::Session))
                .await
                .unwrap();
            s.begin().await.unwrap();
            s.savepoint("sp").await.unwrap();
            assert_refused(
                drain(&mut s, WRITE, None).await,
                &format!("{id} after SAVEPOINT"),
            );
            s.rollback_to_savepoint("sp").await.unwrap();
            assert_refused(
                drain(&mut s, WRITE, None).await,
                &format!("{id} after ROLLBACK TO"),
            );
            s.rollback().await.unwrap();
            assert!(s.is_read_only());
            assert_eq!(count(&mut s).await, before, "{id}: rows were written");
            Box::new(s).close().await.unwrap();
        }
    }

    /// P1-E review M1: MariaDB `ANALYZE <statement>` executes the statement; MySQL
    /// `EXPLAIN ANALYZE` (and its synonym `DESC ANALYZE`) runs a multi-table `DELETE`'s plan.
    /// The classifier never sees them below the statement they wrap (the production
    /// confirmation of the core is asked for `needs_confirmation` classes), a read-only
    /// session refuses them client-side, and sent past the classifier with a forged `Read`
    /// class they are still rejected engine-side (I3).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn analyze_forms_are_destructive_and_refused_engine_side() {
        async fn victims(s: &mut MySession) -> String {
            let st = Statement::sql("SELECT CAST(COUNT(*) AS CHAR) FROM ninesql_explain_victim");
            let mut stream = s.execute(st, ExecOptions::default()).await.unwrap();
            let mut n = None;
            while let Some(item) = stream.next().await {
                if let ninesql_driver_api::ResultItem::Rows(b) = item.unwrap() {
                    use ninesql_driver_api::arrow_array::cast::AsArray;
                    n = Some(b.column(0).as_string::<i32>().value(0).to_owned());
                }
            }
            n.unwrap()
        }
        let driver = MySqlWireDriver::new();
        for (id, engine, forms) in [
            (
                "mariadb",
                EngineId::MARIADB,
                &[
                    "ANALYZE DELETE FROM ninesql_explain_victim",
                    "ANALYZE FORMAT=JSON DELETE FROM ninesql_explain_victim",
                ][..],
            ),
            (
                "mysql",
                EngineId::MYSQL,
                &[
                    "EXPLAIN ANALYZE DELETE v FROM ninesql_explain_victim v \
                     JOIN (SELECT 1 AS x) d ON v.a = d.x",
                    "DESC ANALYZE DELETE v FROM ninesql_explain_victim v \
                     JOIN (SELECT 1 AS x) d ON v.a = d.x",
                ][..],
            ),
        ] {
            let Some(params) = dev_params(id, engine.clone()) else {
                continue;
            };
            let mut w = driver
                .open(dev_params(id, engine.clone()).unwrap())
                .await
                .unwrap();
            for sql in [
                // A table of its own (no shared schema or database is created here).
                "CREATE TABLE IF NOT EXISTS ninesql_explain_victim (a INT)",
                "DELETE FROM ninesql_explain_victim WHERE 1 = 1",
                "INSERT INTO ninesql_explain_victim VALUES (1), (2)",
            ] {
                drain(&mut w, sql, None).await.unwrap();
            }
            for sql in forms {
                let class = w.dialect().classify(sql);
                assert!(
                    class >= StatementClass::Destructive,
                    "{id} {sql}: {class:?}"
                );
                assert!(class.needs_confirmation() && class.is_write(), "{id} {sql}");
            }
            if id == "mariadb" {
                // Why it matters: MariaDB really deletes the rows.
                w.begin().await.unwrap();
                drain(&mut w, forms[0], None).await.unwrap();
                assert_eq!(victims(&mut w).await, "0", "ANALYZE DELETE did not run");
                w.rollback().await.unwrap();
                assert_eq!(victims(&mut w).await, "2");
            }
            Box::new(w).close().await.unwrap();

            let mut p = params;
            p.read_only = true;
            let mut s = driver.open(p).await.unwrap();
            for sql in forms {
                assert!(
                    matches!(
                        drain(&mut s, sql, None).await,
                        Err(DriverError::ReadOnly(_))
                    ),
                    "{id} {sql}: not refused client-side"
                );
                let r = drain(&mut s, sql, Some(StatementClass::Read)).await;
                assert!(
                    matches!(r, Err(DriverError::ReadOnly(_))),
                    "{id} {sql}: not refused engine-side: {r:?}"
                );
                assert!(s.is_read_only());
            }
            assert_eq!(victims(&mut s).await, "2", "{id}: rows were deleted");
            Box::new(s).close().await.unwrap();
            let mut w = driver.open(dev_params(id, engine).unwrap()).await.unwrap();
            drain(&mut w, "DROP TABLE ninesql_explain_victim", None)
                .await
                .unwrap();
            Box::new(w).close().await.unwrap();
        }
    }
}
