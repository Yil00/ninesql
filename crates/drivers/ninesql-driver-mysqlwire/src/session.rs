//! [`Session`] and [`Metadata`] of the mysqlwire driver: thin fronts that check the contract
//! (one statement, classification, read-only refusal, busy state) and send commands to the
//! connection task.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use ninesql_driver_api::{
    CancelHandle, Capabilities, ColumnInfo, Dialect, DriverError, DriverResult, ExecOptions,
    ExecutionTracker, Feature, IsolationLevel, Metadata, ObjectDetails, ObjectInfo, ObjectKind,
    ObjectPath, Page, PageRequest, QueryLanguage, ResultItem, ResultStream, Session, Statement,
    StatementClass, TableInfo, ensure_single_statement, escape_like,
};
use tokio::sync::{mpsc, oneshot};

use crate::actor::{Chunk, Command, Demand, ExecRequest, Rows, Shared};
use crate::cancel::{Control, ExecCancel, MyCancel};
use crate::dialect::{MyDialect, literal, params_supported};

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
        if self.shared.closed.load(Ordering::SeqCst) {
            return Err(self.shared.closed_error());
        }
        let guard = self.tracker.begin()?;
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Command::Internal {
                sql: sql.to_owned(),
                verify,
                guard,
                reply,
            })
            .await
            .map_err(|_| self.shared.closed_error())?;
        // The connection task bounds the statement itself; this also bounds a queue that
        // never drains (a stuck task). Past it the session is closed.
        match tokio::time::timeout(crate::actor::INTERNAL_TIMEOUT * 2, rx).await {
            Ok(r) => r.map_err(|_| self.shared.closed_error())?,
            Err(_) => {
                self.shared.mark_closed("driver statement timed out");
                Err(self.shared.closed_error())
            }
        }
    }
}

/// A MySQL or MariaDB session.
pub struct MySession {
    pub(crate) link: Link,
    pub(crate) capabilities: Capabilities,
    pub(crate) dialect: Arc<MyDialect>,
    pub(crate) control: Arc<Control>,
    /// Statement text may be logged at `trace` level (`ConnectParams::may_log_statements`:
    /// the connection type allows it and the connection is not production, ADR-0007 §6).
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
    async fn dispatch(
        &mut self,
        statement: Statement,
        options: ExecOptions,
        class: StatementClass,
    ) -> DriverResult<ResultStream> {
        let shared = &self.link.shared;
        let text = self.dialect.bind(&statement.text, &statement.params)?;
        if self.log_statements {
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
        let check = format!("SELECT @@SESSION.{}", self.link.shared.ro_var);
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

/// Navigator metadata, served on the session's connection: a call made while a result streams
/// fails with [`DriverError::Busy`]. User input (filter, names, cursor) only reaches the
/// statement as hexadecimal literals, compared as binary strings, so neither the session's
/// `sql_mode` nor its character set or collations change what the driver's SQL means.
pub struct MyMetadata {
    link: Link,
}

impl std::fmt::Debug for MyMetadata {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MyMetadata").finish_non_exhaustive()
    }
}

/// A binary-string literal of `s` (inert in any `sql_mode`).
fn bin(s: &str) -> String {
    if s.is_empty() {
        return "X''".to_owned();
    }
    literal(&ninesql_driver_api::Param::Bytes(s.as_bytes().to_vec()))
        .unwrap_or_else(|_| "X''".to_owned())
}

fn segment(parent: &ObjectPath, idx: usize, kinds: &[ObjectKind]) -> DriverResult<String> {
    parent
        .0
        .get(idx)
        .filter(|s| kinds.contains(&s.kind))
        .map(|s| s.name.clone())
        .ok_or_else(|| DriverError::InvalidParams("unexpected object path".into()))
}

fn cell(row: &[Option<String>], i: usize) -> Option<String> {
    row.get(i).cloned().flatten()
}

/// `AND CAST(col AS BINARY) LIKE <filter> ESCAPE '!'` for a filter, empty otherwise.
fn filter_clause(col: &str, filter: Option<&str>) -> String {
    match filter {
        Some(f) => format!(
            " AND CAST({col} AS BINARY) LIKE {} ESCAPE '!'",
            bin(&escape_like(f, '!'))
        ),
        None => String::new(),
    }
}

/// `AND CAST(key AS BINARY) > <cursor>` for a cursor, empty otherwise.
fn cursor_clause(key: &str, cursor: Option<&str>) -> String {
    match cursor {
        Some(c) => format!(" AND CAST({key} AS BINARY) > {}", bin(c)),
        None => String::new(),
    }
}

#[async_trait::async_trait]
impl Metadata for MyMetadata {
    fn child_kinds(&self, parent: Option<ObjectKind>) -> Vec<ObjectKind> {
        match parent {
            None => vec![ObjectKind::Schema],
            Some(ObjectKind::Schema) => {
                vec![ObjectKind::Table, ObjectKind::View, ObjectKind::Routine]
            }
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
        let limit = page.limit.clamp(1, 10_000);
        let fetch = u64::from(limit) + 1;
        let filter = page.filter.as_deref();
        let cursor = page.cursor.as_deref();
        let sql = match (parent.kind(), kind) {
            (None, ObjectKind::Schema) => format!(
                "SELECT SCHEMA_NAME, NULL, CAST(SCHEMA_NAME AS BINARY) \
                 FROM information_schema.SCHEMATA WHERE TRUE{}{} \
                 ORDER BY CAST(SCHEMA_NAME AS BINARY) LIMIT {fetch}",
                filter_clause("SCHEMA_NAME", filter),
                cursor_clause("SCHEMA_NAME", cursor),
            ),
            (Some(ObjectKind::Schema), ObjectKind::Table | ObjectKind::View) => {
                let schema = segment(parent, 0, &[ObjectKind::Schema])?;
                let types = if kind == ObjectKind::Table {
                    "'BASE TABLE', 'SYSTEM VERSIONED'"
                } else {
                    "'VIEW', 'SYSTEM VIEW'"
                };
                format!(
                    "SELECT TABLE_NAME, TABLE_COMMENT, CAST(TABLE_NAME AS BINARY), TABLE_ROWS, \
                     TABLE_TYPE FROM information_schema.TABLES \
                     WHERE CAST(TABLE_SCHEMA AS BINARY) = {} AND TABLE_TYPE IN ({types}){}{} \
                     ORDER BY CAST(TABLE_NAME AS BINARY) LIMIT {fetch}",
                    bin(&schema),
                    filter_clause("TABLE_NAME", filter),
                    cursor_clause("TABLE_NAME", cursor),
                )
            }
            (Some(ObjectKind::Schema), ObjectKind::Routine) => {
                let schema = segment(parent, 0, &[ObjectKind::Schema])?;
                // Functions and procedures may share a name: the key adds the type.
                format!(
                    "SELECT ROUTINE_NAME, ROUTINE_COMMENT, \
                     CONCAT(CAST(ROUTINE_NAME AS BINARY), X'00', CAST(ROUTINE_TYPE AS BINARY)) \
                     FROM information_schema.ROUTINES \
                     WHERE CAST(ROUTINE_SCHEMA AS BINARY) = {}{}{} \
                     ORDER BY CONCAT(CAST(ROUTINE_NAME AS BINARY), X'00', \
                     CAST(ROUTINE_TYPE AS BINARY)) LIMIT {fetch}",
                    bin(&schema),
                    filter_clause("ROUTINE_NAME", filter),
                    cursor_clause(
                        "CONCAT(CAST(ROUTINE_NAME AS BINARY), X'00', CAST(ROUTINE_TYPE AS BINARY))",
                        cursor
                    ),
                )
            }
            (Some(ObjectKind::Table | ObjectKind::View), ObjectKind::Column) => {
                let schema = segment(parent, 0, &[ObjectKind::Schema])?;
                let table = segment(parent, 1, &[ObjectKind::Table, ObjectKind::View])?;
                let after = match cursor {
                    Some(c) => {
                        let n: u32 = c.parse().map_err(|_| {
                            DriverError::InvalidParams("invalid page cursor".into())
                        })?;
                        format!(" AND ORDINAL_POSITION > {n}")
                    }
                    None => String::new(),
                };
                format!(
                    "SELECT COLUMN_NAME, COLUMN_COMMENT, ORDINAL_POSITION, COLUMN_TYPE, \
                     IS_NULLABLE, COLUMN_DEFAULT, COLUMN_KEY, EXTRA \
                     FROM information_schema.COLUMNS \
                     WHERE CAST(TABLE_SCHEMA AS BINARY) = {} \
                     AND CAST(TABLE_NAME AS BINARY) = {}{}{after} \
                     ORDER BY ORDINAL_POSITION LIMIT {fetch}",
                    bin(&schema),
                    bin(&table),
                    filter_clause("COLUMN_NAME", filter),
                )
            }
            _ => return Err(DriverError::Unsupported(Feature::Metadata(kind))),
        };
        let mut rows = self.link.internal(&sql, false).await?;
        let more = rows.len() > limit as usize;
        rows.truncate(limit as usize);
        let mut items = Vec::with_capacity(rows.len());
        let mut next_cursor = None;
        for row in &rows {
            let name = cell(row, 0).unwrap_or_default();
            let comment = cell(row, 1).filter(|c| !c.is_empty());
            let details = match kind {
                ObjectKind::Table | ObjectKind::View => {
                    let mut t = TableInfo::default();
                    t.estimated_rows = cell(row, 3).and_then(|v| v.parse().ok());
                    t.flavour = match cell(row, 4).as_deref() {
                        Some("SYSTEM VERSIONED") => Some("system-versioned".to_owned()),
                        Some("SYSTEM VIEW") => Some("system".to_owned()),
                        _ => None,
                    };
                    ObjectDetails::Table(t)
                }
                ObjectKind::Column => {
                    let ordinal = cell(row, 2).and_then(|v| v.parse().ok()).unwrap_or(0);
                    let mut c = ColumnInfo::new(ordinal, cell(row, 3).unwrap_or_default());
                    c.nullable = cell(row, 4).as_deref() == Some("YES");
                    c.default = cell(row, 5);
                    c.primary_key = cell(row, 6).as_deref() == Some("PRI");
                    let extra = cell(row, 7).unwrap_or_default().to_ascii_lowercase();
                    c.auto_generated = extra.contains("auto_increment")
                        || extra.contains("virtual")
                        || extra.contains("stored")
                        || extra.contains("persistent");
                    next_cursor = Some(ordinal.to_string());
                    ObjectDetails::Column(c)
                }
                _ => ObjectDetails::None,
            };
            if kind != ObjectKind::Column {
                next_cursor = cell(row, 2);
            }
            items.push(ObjectInfo {
                path: parent.child(kind, name),
                comment,
                details,
            });
        }
        Ok(Page {
            items,
            next_cursor: if more { next_cursor } else { None },
        })
    }

    async fn ddl(&self, _object: &ObjectPath) -> DriverResult<String> {
        Err(DriverError::Unsupported(Feature::Ddl))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_input_becomes_hex() {
        assert_eq!(bin("a'b"), "X'612762'");
        assert_eq!(bin(""), "X''");
        assert_eq!(
            filter_clause("TABLE_NAME", Some("a_%'")),
            " AND CAST(TABLE_NAME AS BINARY) LIKE X'2561215F21252725' ESCAPE '!'"
        );
        assert!(cursor_clause("k", None).is_empty());
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
