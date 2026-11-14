//! [`Session`] and [`Metadata`] of the pgwire driver: thin fronts that check the contract
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

use crate::actor::{Chunk, Command, Demand, ExecRequest, Rows, Shared, params_supported};
use crate::cancel::{CancelTarget, ExecCancel, PgCancel};
use crate::dialect::PgDialect;

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
        if self.shared.closed.load(Ordering::SeqCst) {
            return Err(self.shared.closed_error());
        }
        let guard = self.tracker.begin()?;
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Command::Internal {
                sql: sql.to_owned(),
                params,
                max_rows,
                verify: true,
                guard,
                reply,
            })
            .await
            .map_err(|_| self.shared.closed_error())?;
        rx.await.map_err(|_| self.shared.closed_error())?
    }
}

/// A PostgreSQL session.
pub struct PgSession {
    pub(crate) link: Link,
    pub(crate) capabilities: Capabilities,
    pub(crate) dialect: Arc<PgDialect>,
    pub(crate) cancel_target: Arc<CancelTarget>,
    pub(crate) auto_commit: bool,
    pub(crate) production: bool,
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
    fn engine_cancel(&self, exec: Arc<ExecCancel>) -> Arc<dyn CancelHandle> {
        Arc::new(PgCancel {
            target: Arc::clone(&self.cancel_target),
            exec,
        })
    }

    async fn run(&self, sql: &str) -> DriverResult<Rows> {
        self.link.internal(sql, Vec::new(), 0).await
    }

    /// Applies (or lifts) the engine-side read-only mode and verifies it by reading it back.
    pub(crate) async fn apply_read_only(&mut self, read_only: bool) -> DriverResult<()> {
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
        if !self.production {
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
        if auto_commit && !self.auto_commit && self.in_transaction() {
            // JDBC semantics: switching to auto-commit commits the open transaction.
            self.commit().await?;
        }
        self.auto_commit = auto_commit;
        Ok(())
    }

    async fn begin(&mut self) -> DriverResult<()> {
        self.run("BEGIN").await.map(drop)
    }

    async fn commit(&mut self) -> DriverResult<()> {
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

/// Navigator metadata, served on the session's connection: a call made while a result streams
/// fails with [`DriverError::Busy`]. User input (filter, names, cursor) is always bound.
pub struct PgMetadata {
    link: Link,
}

impl std::fmt::Debug for PgMetadata {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgMetadata").finish_non_exhaustive()
    }
}

const SCHEMAS_SQL: &str = "\
SELECT n.nspname, pg_catalog.obj_description(n.oid, 'pg_namespace') \
FROM pg_catalog.pg_namespace n \
WHERE n.nspname !~ '^pg_toast' AND n.nspname !~ '^pg_temp_' \
  AND ($1::text IS NULL OR n.nspname LIKE $1 ESCAPE '!') \
  AND ($2::text IS NULL OR n.nspname COLLATE \"C\" > $2::text COLLATE \"C\") \
ORDER BY n.nspname COLLATE \"C\" LIMIT $3::int8";

const RELATIONS_SQL: &str = "\
SELECT c.relname, pg_catalog.obj_description(c.oid, 'pg_class'), c.reltuples::int8::text, \
       c.relkind::text \
FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
WHERE n.nspname = $4 AND c.relkind::text = ANY($5::text[]) \
  AND ($1::text IS NULL OR c.relname LIKE $1 ESCAPE '!') \
  AND ($2::text IS NULL OR c.relname COLLATE \"C\" > $2::text COLLATE \"C\") \
ORDER BY c.relname COLLATE \"C\" LIMIT $3::int8";

const COLUMNS_SQL: &str = "\
SELECT a.attname, pg_catalog.col_description(a.attrelid, a.attnum), a.attnum::text, \
       pg_catalog.format_type(a.atttypid, a.atttypmod), (NOT a.attnotnull)::text, \
       pg_catalog.pg_get_expr(d.adbin, d.adrelid), \
       EXISTS (SELECT 1 FROM pg_catalog.pg_index i WHERE i.indrelid = a.attrelid \
               AND i.indisprimary AND a.attnum = ANY(i.indkey))::text, \
       (a.attidentity <> '' OR a.attgenerated <> '' \
        OR coalesce(pg_catalog.pg_get_expr(d.adbin, d.adrelid), '') LIKE 'nextval(%')::text \
FROM pg_catalog.pg_attribute a \
JOIN pg_catalog.pg_class c ON c.oid = a.attrelid \
JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
LEFT JOIN pg_catalog.pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
WHERE n.nspname = $4 AND c.relname = $5 AND a.attnum > 0 AND NOT a.attisdropped \
  AND ($1::text IS NULL OR a.attname LIKE $1 ESCAPE '!') \
  AND ($2::int4 IS NULL OR a.attnum > $2::int4) \
ORDER BY a.attnum LIMIT $3::int8";

const ROUTINES_SQL: &str = "\
SELECT r.name, r.comment FROM ( \
  SELECT p.proname || '(' || pg_catalog.pg_get_function_identity_arguments(p.oid) || ')' AS name, \
         pg_catalog.obj_description(p.oid, 'pg_proc') AS comment, p.proname \
  FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace \
  WHERE n.nspname = $4 AND p.prokind IN ('f', 'p')) r \
WHERE ($1::text IS NULL OR r.proname LIKE $1 ESCAPE '!') \
  AND ($2::text IS NULL OR r.name COLLATE \"C\" > $2::text COLLATE \"C\") \
ORDER BY r.name COLLATE \"C\" LIMIT $3::int8";

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

fn truthy(row: &[Option<String>], i: usize) -> bool {
    cell(row, i).as_deref() == Some("true")
}

#[async_trait::async_trait]
impl Metadata for PgMetadata {
    fn child_kinds(&self, parent: Option<ObjectKind>) -> Vec<ObjectKind> {
        match parent {
            None => vec![ObjectKind::Schema],
            Some(ObjectKind::Schema) => vec![
                ObjectKind::Table,
                ObjectKind::View,
                ObjectKind::MaterializedView,
                ObjectKind::Routine,
            ],
            Some(ObjectKind::Table | ObjectKind::View | ObjectKind::MaterializedView) => {
                vec![ObjectKind::Column]
            }
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
        let filter = page.filter.as_deref().map(|f| escape_like(f, '!'));
        let fetch = (u64::from(limit) + 1).to_string();
        let mut params = vec![filter, page.cursor.clone(), Some(fetch)];
        let sql = match (parent.kind(), kind) {
            (None, ObjectKind::Schema) => SCHEMAS_SQL,
            (
                Some(ObjectKind::Schema),
                ObjectKind::Table | ObjectKind::View | ObjectKind::MaterializedView,
            ) => {
                params.push(Some(segment(parent, 0, &[ObjectKind::Schema])?));
                params.push(Some(
                    match kind {
                        ObjectKind::Table => "{r,p,f}",
                        ObjectKind::View => "{v}",
                        _ => "{m}",
                    }
                    .to_owned(),
                ));
                RELATIONS_SQL
            }
            (Some(ObjectKind::Schema), ObjectKind::Routine) => {
                params.push(Some(segment(parent, 0, &[ObjectKind::Schema])?));
                ROUTINES_SQL
            }
            (
                Some(ObjectKind::Table | ObjectKind::View | ObjectKind::MaterializedView),
                ObjectKind::Column,
            ) => {
                params.push(Some(segment(parent, 0, &[ObjectKind::Schema])?));
                params.push(Some(segment(
                    parent,
                    1,
                    &[
                        ObjectKind::Table,
                        ObjectKind::View,
                        ObjectKind::MaterializedView,
                    ],
                )?));
                if let Some(c) = &page.cursor
                    && c.parse::<i32>().is_err()
                {
                    return Err(DriverError::InvalidParams("invalid page cursor".into()));
                }
                COLUMNS_SQL
            }
            _ => return Err(DriverError::Unsupported(Feature::Metadata(kind))),
        };
        let mut rows = self.link.internal(sql, params, 0).await?;
        let more = rows.len() > limit as usize;
        rows.truncate(limit as usize);
        let mut items = Vec::with_capacity(rows.len());
        let mut next_cursor = None;
        for row in &rows {
            let name = cell(row, 0).unwrap_or_default();
            let comment = cell(row, 1);
            let details = match kind {
                ObjectKind::Table | ObjectKind::View | ObjectKind::MaterializedView => {
                    let mut t = TableInfo::default();
                    t.estimated_rows = cell(row, 2)
                        .and_then(|v| v.parse::<i64>().ok())
                        .and_then(|v| u64::try_from(v).ok());
                    t.flavour = match cell(row, 3).as_deref() {
                        Some("p") => Some("partitioned".to_owned()),
                        Some("f") => Some("foreign".to_owned()),
                        _ => None,
                    };
                    ObjectDetails::Table(t)
                }
                ObjectKind::Column => {
                    let ordinal = cell(row, 2).and_then(|v| v.parse().ok()).unwrap_or(0);
                    let mut c = ColumnInfo::new(ordinal, cell(row, 3).unwrap_or_default());
                    c.nullable = truthy(row, 4);
                    c.default = cell(row, 5);
                    c.primary_key = truthy(row, 6);
                    c.auto_generated = truthy(row, 7);
                    next_cursor = Some(ordinal.to_string());
                    ObjectDetails::Column(c)
                }
                _ => ObjectDetails::None,
            };
            if kind != ObjectKind::Column {
                next_cursor = Some(name.clone());
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
    fn call_detection() {
        assert!(is_call("CALL p()"));
        assert!(is_call("  call\tp()"));
        assert!(!is_call("callme()"));
        assert!(!is_call("SELECT 1"));
        assert!(!is_call("cal"));
    }
}
