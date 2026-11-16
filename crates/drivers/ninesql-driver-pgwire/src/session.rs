//! [`Session`] of the pgwire driver: thin fronts that check the contract
//! (one statement, classification, read-only refusal, busy state) and send commands to the
//! connection task.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use ninesql_driver_api::{
    CancelHandle, Capabilities, Dialect, DriverError, DriverResult, ExecOptions, ExecutionTracker,
    Feature, IsolationLevel, Metadata, QueryLanguage, ResultItem, ResultStream, Session, Statement,
    StatementClass, ensure_single_statement,
};
use tokio::sync::{mpsc, oneshot};

use crate::actor::{Chunk, Command, Demand, ExecRequest, Rows, Shared, params_supported};
use crate::cancel::{CancelTarget, ExecCancel, MetaGate, PgCancel};
use crate::dialect::PgDialect;
use crate::engine::{self, Flavor, ServerInfo};
use crate::metadata::PgMetadata;

/// Handles shared by the session and its metadata.
#[derive(Clone)]
pub(crate) struct Link {
    pub(crate) tx: mpsc::Sender<Command>,
    pub(crate) shared: Arc<Shared>,
    pub(crate) tracker: ExecutionTracker,
}

impl Link {
    /// Runs a driver statement (text results), as one execution of the session.
    pub(crate) async fn internal(
        &self,
        sql: &str,
        params: Vec<Option<String>>,
        max_rows: i32,
    ) -> DriverResult<Rows> {
        self.internal_limited(sql, params, max_rows, None, None)
            .await
    }

    /// [`internal`](Self::internal) with a value limit (`Command::Internal::value_limit`) and,
    /// for navigator calls, the cancel target: if the caller drops the future before the
    /// answer, the statement is cancelled server-side (scoped to this execution).
    pub(crate) async fn internal_limited(
        &self,
        sql: &str,
        params: Vec<Option<String>>,
        max_rows: i32,
        value_limit: Option<usize>,
        cancel_on_drop: Option<&Arc<CancelTarget>>,
    ) -> DriverResult<Rows> {
        if self.shared.closed.load(Ordering::SeqCst) {
            return Err(self.shared.closed_error());
        }
        let guard = self.tracker.begin()?;
        let gate = cancel_on_drop.map(|_| Arc::new(MetaGate::default()));
        let mut on_drop = cancel_on_drop
            .zip(gate.as_ref())
            .map(|(target, gate)| CancelOnDrop {
                tracker: self.tracker.clone(),
                id: guard.id(),
                target: Arc::clone(target),
                gate: Arc::clone(gate),
                armed: true,
            });
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Command::Internal {
                sql: sql.to_owned(),
                params,
                max_rows,
                value_limit,
                gate,
                verify: true,
                guard,
                reply,
            })
            .await
            .map_err(|_| self.shared.closed_error())?;
        let result = rx.await.map_err(|_| self.shared.closed_error());
        // Answered: nothing left to cancel.
        if let Some(d) = on_drop.as_mut() {
            d.armed = false;
        }
        result?
    }
}

/// Cancels execution `id` server-side when dropped (a navigator call abandoned by its caller,
/// for example by the core's timeout). Scoped by the tracker: a no-op once `id` has ended.
struct CancelOnDrop {
    tracker: ExecutionTracker,
    id: ninesql_driver_api::ExecutionId,
    target: Arc<CancelTarget>,
    gate: Arc<MetaGate>,
    armed: bool,
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        // Only while the navigator statement itself runs (never its savepoint handling).
        if !self.armed || !self.gate.abandon() {
            return;
        }
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let handle = self.tracker.cancel_handle(
            self.id,
            Arc::new(PgCancel {
                target: Arc::clone(&self.target),
                exec: Arc::new(ExecCancel::default()),
            }),
        );
        rt.spawn(async move {
            let _ = handle.cancel().await;
        });
    }
}

/// A PostgreSQL session.
pub struct PgSession {
    pub(crate) link: Link,
    pub(crate) capabilities: Capabilities,
    pub(crate) dialect: Arc<PgDialect>,
    pub(crate) cancel_target: Arc<CancelTarget>,
    pub(crate) auto_commit: bool,
    /// Statement text may be logged at `trace` level (`ConnectParams::may_log_statements`:
    /// the connection type allows it and the connection is not production, ADR-0007 §6), and
    /// only a text that holds no credential (`may_log_statement_text`, I1).
    pub(crate) log_statements: bool,
    /// Cancel flag of the current execution, shared with `Session::cancel_handle`.
    pub(crate) current_exec: std::sync::Mutex<Arc<ExecCancel>>,
}

impl std::fmt::Debug for PgSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgSession")
            .field("server_version", &self.capabilities.server_version)
            .field("auto_commit", &self.auto_commit)
            .field("read_only", &self.is_read_only())
            .finish_non_exhaustive()
    }
}

/// `true` when the statement starts with `CALL` (procedures may commit: never wrapped in a
/// driver transaction).
fn is_call(text: &str) -> bool {
    let t = text.trim_start();
    t.get(..4).is_some_and(|w| w.eq_ignore_ascii_case("call"))
        && t[4..].starts_with(char::is_whitespace)
}

impl PgSession {
    /// Sends a statement that passed the classify gate of `Session::execute` (this crate's
    /// tests also use it to skip the gate). Private: `class` is the dialect's classification.
    async fn dispatch(
        &mut self,
        statement: Statement,
        options: ExecOptions,
        class: StatementClass,
    ) -> DriverResult<ResultStream> {
        let shared = &self.link.shared;
        // Never a text that may hold a credential, init SQL included (I1).
        if ninesql_driver_api::may_log_statement_text(self.log_statements, &statement.text) {
            tracing::trace!(statement = %statement.text, "pgwire execute");
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
            is_call: is_call(&statement.text),
            auto_begin: !self.auto_commit && class != StatementClass::Transaction,
            text: statement.text,
            params: statement.params,
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
        Arc::new(PgCancel {
            target: Arc::clone(&self.cancel_target),
            exec,
        })
    }

    async fn run(&self, sql: &str) -> DriverResult<Rows> {
        self.link.internal(sql, Vec::new(), 0).await
    }

    fn flavor(&self) -> Flavor {
        self.link.shared.flavor
    }

    /// QuestDB has no transactions in the PostgreSQL sense (I8).
    fn refuse_on_questdb(&self) -> DriverResult<()> {
        if self.flavor() == Flavor::QuestDb {
            Err(DriverError::Unsupported(Feature::Transactions))
        } else {
            Ok(())
        }
    }

    /// Checks the server against the declared engine from inside the session (`version()`,
    /// the `timescaledb` extension) and reads what the exact capabilities depend on. A mismatch
    /// fails the connect (docs/08, P4-C).
    pub(crate) async fn check_server(&mut self, info: &mut ServerInfo) -> DriverResult<()> {
        let flavor = self.flavor();
        let first = |rows: Rows| {
            rows.into_iter()
                .next()
                .and_then(|r| r.into_iter().next())
                .flatten()
        };
        let version_fn = first(self.run("SELECT pg_catalog.version()").await?).unwrap_or_default();
        let param = self.capabilities.server_version.clone().unwrap_or_default();
        engine::check_family(
            flavor,
            engine::server_family(&param, Some(&version_fn)),
            &version_fn,
        )?;
        match flavor {
            Flavor::Postgres | Flavor::Timescale => {
                let ts = first(
                    self.run(
                        "SELECT e.extversion::pg_catalog.text FROM pg_catalog.pg_extension e \
                         WHERE e.extname OPERATOR(pg_catalog.=) 'timescaledb'::pg_catalog.name",
                    )
                    .await?,
                );
                if flavor == Flavor::Timescale {
                    if ts.is_none() {
                        return Err(DriverError::Connect(
                            "this database has no timescaledb extension, so it is not \
                             TimescaleDB; choose PostgreSQL, or create the extension first"
                                .into(),
                        ));
                    }
                    info.timescale_version = ts;
                }
            }
            Flavor::Yugabyte => {
                // Without its read-committed mode YugabyteDB runs READ COMMITTED as snapshot
                // isolation: ask the server what a READ COMMITTED transaction really runs.
                self.run("BEGIN ISOLATION LEVEL READ COMMITTED").await?;
                let effective = self
                    .run(
                        "SELECT pg_catalog.current_setting(\
                         'yb_effective_transaction_isolation_level'::pg_catalog.text, true)",
                    )
                    .await;
                self.run("ROLLBACK").await?;
                info.yb_read_committed =
                    first(effective?).is_some_and(|v| v.eq_ignore_ascii_case("read committed"));
            }
            Flavor::QuestDb => {}
        }
        Ok(())
    }

    /// QuestDB: no session read-only mode. A session is read-only only when the server makes it
    /// so (its read-only user, or a read-only server), which is checked twice: in the server's
    /// settings, and with a write that changes nothing whatever the account and must fail with
    /// a permission error. Anything else is refused, never faked (I3, I8).
    async fn apply_questdb_read_only(&mut self, read_only: bool) -> DriverResult<()> {
        if !read_only {
            return Err(DriverError::ReadOnly(
                "QuestDB's read-only mode belongs to the account or the server; connect as a \
                 user that may write instead"
                    .into(),
            ));
        }
        let refused = || {
            DriverError::ReadOnly(
                "QuestDB has no session read-only mode and this account can write, so the \
                 read-only session was refused; connect as QuestDB's read-only user \
                 (pg.readonly.user) or to a read-only server (pg.security.readonly)"
                    .into(),
            )
        };
        let settings: Vec<(String, String)> = self
            .run(engine::QUESTDB_READ_ONLY_SETTINGS_SQL)
            .await
            .map_err(|_| refused())?
            .into_iter()
            .filter_map(|r| {
                let mut it = r.into_iter();
                Some((it.next().flatten()?, it.next().flatten()?))
            })
            .collect();
        let user = self
            .run("SELECT current_user()")
            .await
            .map_err(|_| refused())?
            .into_iter()
            .next()
            .and_then(|r| r.into_iter().next())
            .flatten()
            .unwrap_or_default();
        let denied = match self.run(engine::QUESTDB_WRITE_PROBE_SQL).await {
            Err(DriverError::Query { message, .. }) => engine::questdb_probe_denied(&message),
            Err(DriverError::ReadOnly(_)) => true,
            _ => false,
        };
        if engine::questdb_settings_read_only(&settings, &user) && denied {
            self.link.shared.read_only.store(true, Ordering::SeqCst);
            Ok(())
        } else {
            Err(refused())
        }
    }

    /// Applies (or lifts) the engine-side read-only mode and verifies it by reading it back.
    pub(crate) async fn apply_read_only(&mut self, read_only: bool) -> DriverResult<()> {
        if self.flavor() == Flavor::QuestDb {
            return self.apply_questdb_read_only(read_only).await;
        }
        let sql = if read_only {
            "SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY"
        } else {
            "SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE"
        };
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
        let rows = self.run("SHOW default_transaction_read_only").await?;
        let value = rows
            .first()
            .and_then(|r| r.first())
            .and_then(Option::as_deref);
        let expected = if read_only { "on" } else { "off" };
        if value == Some(expected) {
            self.link
                .shared
                .read_only
                .store(read_only, Ordering::SeqCst);
            Ok(())
        } else {
            self.link.shared.read_only.store(was, Ordering::SeqCst);
            Err(DriverError::ReadOnly(
                "the server did not confirm the read-only setting".into(),
            ))
        }
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
impl Session for PgSession {
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
        Arc::new(PgMetadata {
            link: self.link.clone(),
            cancel_target: Arc::clone(&self.cancel_target),
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
        // One statement per protocol call, with the session's current lexical rules.
        ensure_single_statement(self.dialect.as_ref(), &statement.text)?;
        let class = self.dialect.classify(&statement.text);
        if class == StatementClass::Transaction && self.flavor() == Flavor::QuestDb {
            return Err(DriverError::Unsupported(Feature::Transactions));
        }
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
        self.auto_commit
    }

    fn in_transaction(&self) -> bool {
        self.link.shared.in_transaction()
    }

    async fn set_auto_commit(&mut self, auto_commit: bool) -> DriverResult<()> {
        if !auto_commit {
            self.refuse_on_questdb()?;
        }
        if auto_commit && !self.auto_commit && self.in_transaction() {
            // JDBC semantics: switching to auto-commit commits the open transaction.
            self.commit().await?;
        }
        self.auto_commit = auto_commit;
        Ok(())
    }

    async fn begin(&mut self) -> DriverResult<()> {
        self.refuse_on_questdb()?;
        self.run("BEGIN").await.map(drop)
    }

    async fn commit(&mut self) -> DriverResult<()> {
        if self.flavor() == Flavor::QuestDb && !self.in_transaction() {
            return Ok(());
        }
        let failed = self.link.shared.txn_status.load(Ordering::SeqCst) == b'E';
        self.run("COMMIT").await?;
        if failed {
            return Err(DriverError::Query {
                code: Some("25P02".into()),
                message: "the transaction had failed: COMMIT rolled it back".into(),
                position: None,
            });
        }
        Ok(())
    }

    async fn rollback(&mut self) -> DriverResult<()> {
        if self.flavor() == Flavor::QuestDb && !self.in_transaction() {
            return Ok(());
        }
        self.run("ROLLBACK").await.map(drop)
    }

    async fn savepoint(&mut self, name: &str) -> DriverResult<()> {
        self.refuse_on_questdb()?;
        let sql = format!("SAVEPOINT {}", self.dialect.quote_identifier(name));
        self.run(&sql).await.map(drop)
    }

    async fn rollback_to_savepoint(&mut self, name: &str) -> DriverResult<()> {
        self.refuse_on_questdb()?;
        let sql = format!(
            "ROLLBACK TO SAVEPOINT {}",
            self.dialect.quote_identifier(name)
        );
        self.run(&sql).await.map(drop)
    }

    async fn set_isolation_level(&mut self, level: IsolationLevel) -> DriverResult<()> {
        self.refuse_on_questdb()?;
        if !self
            .capabilities
            .transactions
            .isolation_levels
            .contains(&level)
        {
            return Err(DriverError::Unsupported(Feature::Transactions));
        }
        let level = match level {
            IsolationLevel::ReadCommitted => "READ COMMITTED",
            IsolationLevel::RepeatableRead => "REPEATABLE READ",
            IsolationLevel::Serializable => "SERIALIZABLE",
            // PostgreSQL runs READ UNCOMMITTED as READ COMMITTED: not offered (I8).
            IsolationLevel::ReadUncommitted | IsolationLevel::Snapshot => {
                return Err(DriverError::Unsupported(Feature::Transactions));
            }
        };
        let sql = format!("SET SESSION CHARACTERISTICS AS TRANSACTION ISOLATION LEVEL {level}");
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
        }
        self.link.shared.mark_closed("session closed");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn call_detection() {
        assert!(is_call("CALL p()"));
        assert!(is_call("  call\tp()"));
        assert!(!is_call("callme()"));
        assert!(!is_call("SELECT 1"));
        assert!(!is_call("cal"));
    }
}

/// Security review follow-up: the driver's `SET TRANSACTION READ ONLY` re-assert, tested past
/// the classifier. The one-shot `READ WRITE` comma form is sent through `dispatch` with a
/// forged `Session` class, as a classifier gap would let it through; a following write must
/// still fail engine-side. Skipped when the dev PostgreSQL is unreachable, unless
/// `NINESQL_REQUIRE_ENGINES` lists `postgresql`.
#[cfg(test)]
mod read_only_reassert {
    use std::time::Duration;

    use ninesql_driver_api::{
        Auth, ConnectParams, Endpoint, EngineId, ExecOptions, Secret, Session, Statement,
        StatementClass, TlsMode,
    };

    use super::PgSession;
    use crate::PgWireDriver;

    const ONE_SHOT: &str = "SET TRANSACTION ISOLATION LEVEL READ COMMITTED, READ WRITE";
    const WRITE: &str = "SELECT ninesql_test.reassert_write()";

    fn dev_params() -> Option<ConnectParams> {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../dev/engines.json");
        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let e = doc["engines"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["id"] == "postgresql")
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
                .any(|s| s.trim() == "postgresql");
            assert!(
                !required,
                "postgresql is required but unreachable at {addr}"
            );
            return None;
        }
        let mut p = ConnectParams::new(EngineId::POSTGRESQL, Endpoint::tcp(host, port));
        p.auth = Auth::password(
            e["user"].as_str().unwrap(),
            Some(Secret::new(e["password"].as_str().unwrap().to_owned())),
        );
        p.database = Some(e["database"].as_str().unwrap().to_owned());
        p.tls.mode = TlsMode::Disable;
        p.application_name = "ninesql-pgwire-reassert".into();
        Some(p)
    }

    async fn drain(
        s: &mut PgSession,
        sql: &str,
        class: Option<StatementClass>,
    ) -> Result<(), ninesql_driver_api::DriverError> {
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

    async fn count(s: &mut PgSession) -> i64 {
        let mut stream = s
            .execute(
                Statement::sql("SELECT count(*)::int8 FROM ninesql_test.reassert_written"),
                ExecOptions::default(),
            )
            .await
            .unwrap();
        let mut n = None;
        while let Some(item) = stream.next().await {
            if let ninesql_driver_api::ResultItem::Rows(b) = item.unwrap() {
                use ninesql_driver_api::arrow_array::cast::AsArray;
                n = Some(
                    b.column(0)
                        .as_primitive::<ninesql_driver_api::arrow_array::types::Int64Type>()
                        .value(0),
                );
            }
        }
        n.unwrap()
    }

    fn assert_refused(r: Result<(), ninesql_driver_api::DriverError>, case: &str) {
        assert!(
            matches!(r, Err(ninesql_driver_api::DriverError::ReadOnly(_))),
            "{case}: write after a one-shot READ WRITE sent past the classifier: {r:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn one_shot_read_write_past_the_classifier_cannot_write() {
        let Some(params) = dev_params() else { return };
        let driver = PgWireDriver::new();
        let mut w = driver.open(dev_params().unwrap()).await.unwrap();
        for sql in [
            "CREATE SCHEMA IF NOT EXISTS ninesql_test",
            "CREATE TABLE IF NOT EXISTS ninesql_test.reassert_written (a int)",
            "CREATE OR REPLACE FUNCTION ninesql_test.reassert_write() RETURNS int LANGUAGE sql \
             AS $$ INSERT INTO ninesql_test.reassert_written VALUES (1) RETURNING 1 $$",
        ] {
            drain(&mut w, sql, None).await.unwrap();
        }
        Box::new(w).close().await.unwrap();

        let mut p = params;
        p.read_only = true;
        let mut s = driver.open(p).await.unwrap();
        let before = count(&mut s).await;
        for mode in ["autocommit", "manual", "explicit"] {
            match mode {
                "manual" => s.set_auto_commit(false).await.unwrap(),
                "explicit" => {
                    s.set_auto_commit(true).await.unwrap();
                    s.begin().await.unwrap();
                }
                _ => {}
            }
            // Outside a transaction block PostgreSQL only warns; inside one it is accepted.
            let _ = drain(&mut s, ONE_SHOT, Some(StatementClass::Session)).await;
            assert_refused(drain(&mut s, WRITE, None).await, mode);
            if s.in_transaction() {
                s.rollback().await.unwrap();
            }
            assert!(s.is_read_only());
        }
        s.set_auto_commit(true).await.unwrap();
        // Savepoints: the one-shot, then SAVEPOINT, a write, ROLLBACK TO, a write.
        s.begin().await.unwrap();
        drain(&mut s, ONE_SHOT, Some(StatementClass::Session))
            .await
            .unwrap();
        s.savepoint("sp").await.unwrap();
        assert_refused(drain(&mut s, WRITE, None).await, "after SAVEPOINT");
        s.rollback_to_savepoint("sp").await.unwrap();
        assert_refused(drain(&mut s, WRITE, None).await, "after ROLLBACK TO");
        s.rollback().await.unwrap();
        assert!(s.is_read_only());
        assert_eq!(count(&mut s).await, before, "rows were written");
        Box::new(s).close().await.unwrap();
    }

    /// P1-E review M1: PostgreSQL runs the statement under `EXPLAIN ("analyze")` and its
    /// `U&`-escaped forms. The classifier sees them as the destructive `DELETE` they run (the
    /// production confirmation of the core is asked for `needs_confirmation` classes), a
    /// read-only session refuses them client-side, and sent past the classifier with a forged
    /// `Read` class they are still rejected engine-side (I3). Its own schema: `CREATE SCHEMA IF
    /// NOT EXISTS` is not safe against the concurrent tests of this module (23505).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn explain_analyze_forms_are_destructive_and_refused_engine_side() {
        const FORMS: &[&str] = &[
            "EXPLAIN (\"analyze\") DELETE FROM ninesql_test_explain.victim",
            "EXPLAIN (FORMAT JSON, \"analyze\" on) DELETE FROM ninesql_test_explain.victim",
            "EXPLAIN (U&\"analyze\") DELETE FROM ninesql_test_explain.victim",
            "EXPLAIN (U&\"\\0061nalyze\") DELETE FROM ninesql_test_explain.victim",
        ];
        async fn victims(s: &mut PgSession) -> i64 {
            let mut stream = s
                .execute(
                    Statement::sql("SELECT count(*)::int8 FROM ninesql_test_explain.victim"),
                    ExecOptions::default(),
                )
                .await
                .unwrap();
            let mut n = None;
            while let Some(item) = stream.next().await {
                if let ninesql_driver_api::ResultItem::Rows(b) = item.unwrap() {
                    use ninesql_driver_api::arrow_array::cast::AsArray;
                    n = Some(
                        b.column(0)
                            .as_primitive::<ninesql_driver_api::arrow_array::types::Int64Type>()
                            .value(0),
                    );
                }
            }
            n.unwrap()
        }

        let Some(params) = dev_params() else { return };
        let driver = PgWireDriver::new();
        let mut w = driver.open(dev_params().unwrap()).await.unwrap();
        for sql in [
            "CREATE SCHEMA IF NOT EXISTS ninesql_test_explain",
            "CREATE TABLE IF NOT EXISTS ninesql_test_explain.victim (a int)",
            "TRUNCATE ninesql_test_explain.victim",
            "INSERT INTO ninesql_test_explain.victim VALUES (1), (2)",
        ] {
            drain(&mut w, sql, None).await.unwrap();
        }
        for sql in FORMS {
            let class = w.dialect().classify(sql);
            assert_eq!(class, StatementClass::Destructive, "{sql}");
            assert!(class.needs_confirmation() && class.is_write(), "{sql}");
        }
        // Why it matters: in a writable session the engine really deletes the rows.
        for sql in FORMS {
            w.begin().await.unwrap();
            drain(&mut w, sql, None).await.unwrap();
            assert_eq!(victims(&mut w).await, 0, "{sql} did not run the DELETE");
            w.rollback().await.unwrap();
            assert_eq!(victims(&mut w).await, 2);
        }
        Box::new(w).close().await.unwrap();

        let mut p = params;
        p.read_only = true;
        let mut s = driver.open(p).await.unwrap();
        for sql in FORMS {
            assert!(
                matches!(
                    drain(&mut s, sql, None).await,
                    Err(ninesql_driver_api::DriverError::ReadOnly(_))
                ),
                "{sql}: not refused client-side"
            );
            match drain(&mut s, sql, Some(StatementClass::Read)).await {
                Err(ninesql_driver_api::DriverError::ReadOnly(msg)) => {
                    assert!(msg.contains("read-only"), "{sql}: {msg}");
                }
                other => panic!("{sql}: not refused engine-side: {other:?}"),
            }
            assert!(s.is_read_only());
        }
        assert_eq!(victims(&mut s).await, 2, "rows were deleted");
        Box::new(s).close().await.unwrap();
        let mut w = driver.open(dev_params().unwrap()).await.unwrap();
        drain(&mut w, "DROP SCHEMA ninesql_test_explain CASCADE", None)
            .await
            .unwrap();
        Box::new(w).close().await.unwrap();
    }
}
