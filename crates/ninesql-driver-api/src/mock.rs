//! In-memory mock driver for UI and core tests (feature `mock`). Never shipped in release builds.
//!
//! The mock serves one engine, [`MOCK_ENGINE`], and understands a tiny statement language that
//! looks like SQL so the default [`Dialect::classify`] treats it
//! correctly:
//!
//! | Statement | Result |
//! |---|---|
//! | `SELECT * FROM numbers(N)` | `N` rows (`id: Int64`, `label: Utf8`), generated lazily batch by batch |
//! | `SELECT * FROM slow_numbers(N, MS)` | Same, waiting `MS` ms before each batch (timeout / cancel tests) |
//! | `SELECT * FROM wide(N, BYTES)` | `N` rows with `BYTES`-byte labels, cut at the value budget and flagged |
//! | `SELECT * FROM documents(N)` | `N` documents |
//! | `SELECT * FROM wide_documents(N, BYTES)` | `N` documents with a `BYTES`-byte `payload`, cut at the value / row budget and wrapped in [`DocValue::Truncated`] |
//! | `SELECT 1 AS <identifier>` | One row, one column named after the (ANSI-quoted) identifier (quoting conformance) |
//! | `SELECT * FROM notices(N)` | `N` notices |
//! | `SELECT mock_write()` | A write the classifier cannot see (rejected by the simulated server in read-only mode) |
//! | `SELECT mock_lift_read_only()` | Lifts the simulated engine-side read-only setting (detected; the session is closed) |
//! | `SELECT * FROM multi()` | Two result sets with a notice in between |
//! | `SELECT error()` | A query error with SQLSTATE `XX000` |
//! | `INSERT …`, `UPDATE … WHERE …`, `DELETE … WHERE …`, `CREATE …`, `DROP …` | Update count 1 |
//! | `BEGIN`, `COMMIT`, `ROLLBACK` | Transaction control |
//! | `SET …` | Accepted (refused in read-only mode when it may change the access mode) |
//!
//! Read-only sessions behave like an engine with
//! [`ReadOnlyMechanism::SessionSetting`] (liftable): writes are refused client-side by classification and
//! rejected again by the simulated server. [`MockStats`] exposes counters (rows generated,
//! server-side cancels, connects) so tests can prove that streaming stops at the budget and that
//! cancel reaches the "server".
//!
//! Cancellation behaves like PostgreSQL (M6): the cancel targets "whatever the session runs",
//! the session uses an [`ExecutionTracker`] so handles are scoped to one execution, and a
//! cancelled stream ends only when the end is confirmed: until then the session is busy. The
//! simulated server only works while the stream is polled, so, like a PostgreSQL backend
//! waiting on a suspended portal, it cannot act on a cancel while idle. The cancel handle
//! therefore also sets the driver-local flag of the "Idle cursors" rule
//! ([`crate::result`]): a batch already being fetched finishes and then stops; an idle cursor
//! is closed on the next poll without fetching ([`MockStats::cursors_closed_on_cancel`]). The mock declares
//! [`MOCK_RESERVED_PROPERTY`] as an extra reserved property key and does not retain the secret
//! after connect.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use arrow_array::{ArrayRef, Int64Array, RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema, SchemaRef};

use crate::capabilities::{
    CancelSupport, Capabilities, ConnectCapabilities, EditingCapabilities, ExplainFormat, Feature,
    IsolationLevel, MetadataCapabilities, ObjectKind, QueryLanguage, ReadOnlyMechanism, ResultKind,
    TransactionCapabilities,
};
use crate::dialect::{AnsiDialect, Dialect, StatementClass, ensure_single_statement};
use crate::driver::{Driver, Session};
use crate::error::{DriverError, DriverResult};
use crate::execution::{ExecutionGuard, ExecutionTracker};
use crate::metadata::{
    ColumnInfo, Metadata, ObjectDetails, ObjectInfo, ObjectPath, Page, PageRequest, TableInfo,
};
use crate::params::{Auth, AuthKind, ConnectParams, Endpoint, EngineId, PasswordExchange, TlsMode};
use crate::result::{
    CancelHandle, DocValue, Document, Notice, NoticeSeverity, RawResultStream, ResultItem,
    ResultSetHeader, ResultSetKind, ResultStream, TRUNCATED_VALUES_METADATA_KEY,
};
use crate::secret::Secret;
use crate::statement::{
    BatchLimiter, ExecOptions, FetchBudget, RowBudget, Statement, truncate_str,
};

/// Engine id served by the mock driver.
pub const MOCK_ENGINE: EngineId = EngineId::from_static("mock");

static MOCK_ENGINES: [EngineId; 1] = [MOCK_ENGINE];

/// Extra reserved driver property declared by the mock in
/// [`Capabilities::extra_reserved_properties`].
pub const MOCK_RESERVED_PROPERTY: &str = "mock_backdoor";

/// Host name the mock treats as a TLS endpoint whose certificate is signed by a private CA:
/// with `verify-ca` / `verify-full` it connects only when the CA file is named
/// [`MOCK_TRUSTED_CA_FILE`], and `verify-full` also requires `server_name` (if set) to be this
/// host; failures are [`DriverError::Tls`] (for
/// `conformance::assert_verify_full_rejects_untrusted_certificate`).
pub const MOCK_SELF_SIGNED_HOST: &str = "self-signed.mock.invalid";

/// File name of the CA the mock's TLS endpoint trusts (never read: only the name is checked).
pub const MOCK_TRUSTED_CA_FILE: &str = "mock-ca.pem";

/// Number of tables listed in the mock `public` schema (enough to exercise paging).
pub const MOCK_TABLE_COUNT: usize = 50;

/// Counters shared by a [`MockDriver`] and all its sessions.
#[derive(Debug, Default)]
pub struct MockStats {
    rows_generated: AtomicU64,
    server_cancels: AtomicU64,
    connects: AtomicU64,
    server_rejected_writes: AtomicU64,
    cursors_closed_on_cancel: AtomicU64,
}

impl MockStats {
    /// Rows / documents the simulated server produced (including those not yet consumed).
    #[must_use]
    pub fn rows_generated(&self) -> u64 {
        self.rows_generated.load(Ordering::SeqCst)
    }

    /// Server-side cancel requests received.
    #[must_use]
    pub fn server_cancels(&self) -> u64 {
        self.server_cancels.load(Ordering::SeqCst)
    }

    /// Successful connects.
    #[must_use]
    pub fn connects(&self) -> u64 {
        self.connects.load(Ordering::SeqCst)
    }

    /// Writes rejected by the simulated server's read-only mode (writes that got past the
    /// client-side classification).
    #[must_use]
    pub fn server_rejected_writes(&self) -> u64 {
        self.server_rejected_writes.load(Ordering::SeqCst)
    }

    /// Cursors closed by the raw stream because a cancel flagged it (idle-cursor rule).
    #[must_use]
    pub fn cursors_closed_on_cancel(&self) -> u64 {
        self.cursors_closed_on_cancel.load(Ordering::SeqCst)
    }
}

/// The capabilities the mock declares by default: everything the API can express for a SQL
/// engine, so UI tests see every action. Override with [`MockDriver::with_capabilities`] to test
/// how the UI hides features (I8).
#[must_use]
pub fn full_capabilities() -> Capabilities {
    Capabilities {
        engine: MOCK_ENGINE,
        server_version: None,
        query_languages: BTreeSet::from([QueryLanguage::Sql]),
        result_kinds: BTreeSet::from([ResultKind::Tabular, ResultKind::Documents]),
        metadata: MetadataCapabilities {
            kinds: BTreeSet::from([
                ObjectKind::Catalog,
                ObjectKind::Schema,
                ObjectKind::Table,
                ObjectKind::Column,
            ]),
            ddl: BTreeSet::from([ObjectKind::Table]),
        },
        editing: EditingCapabilities {
            insert: true,
            update: true,
            delete: true,
            requires_unique_key: true,
        },
        transactions: TransactionCapabilities {
            manual_commit: true,
            savepoints: true,
            isolation_levels: BTreeSet::from([
                IsolationLevel::ReadCommitted,
                IsolationLevel::Serializable,
            ]),
        },
        read_only: ReadOnlyMechanism::SessionSetting { liftable: true },
        cancel: CancelSupport::ServerSide,
        explain: BTreeSet::from([ExplainFormat::Text]),
        multiple_result_sets: true,
        server_side_timeout: false,
        tls: BTreeSet::new(),
        parameters: true,
        extra_reserved_properties: BTreeSet::from([MOCK_RESERVED_PROPERTY.to_owned()]),
        retains_secret_after_connect: false,
        connect: ConnectCapabilities {
            // Any key that is not reserved.
            accepted_properties: None,
            auth: BTreeSet::from([AuthKind::None, AuthKind::Password, AuthKind::Token]),
            // The mock has no real exchange: it declares the policy so UI tests see it.
            password_exchanges: BTreeSet::from([
                PasswordExchange::Any,
                PasswordExchange::ScramOnly,
            ]),
            verify_ca_requires_ca_file: true,
            client_certificates: true,
            encrypted_client_keys: true,
        },
    }
}

/// Mock driver. Cheap to clone; clones share [`MockStats`].
#[derive(Clone)]
pub struct MockDriver {
    capabilities: Capabilities,
    password: Option<Arc<Secret>>,
    stats: Arc<MockStats>,
}

impl Default for MockDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for MockDriver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MockDriver")
            .field("engine", &self.capabilities.engine)
            .field("stats", &self.stats)
            .finish_non_exhaustive()
    }
}

impl MockDriver {
    /// A mock with [`full_capabilities`] that accepts any credentials.
    #[must_use]
    pub fn new() -> Self {
        Self {
            capabilities: full_capabilities(),
            password: None,
            stats: Arc::new(MockStats::default()),
        }
    }

    /// Declares other capabilities (the engine id is forced to [`MOCK_ENGINE`]).
    #[must_use]
    pub fn with_capabilities(mut self, mut capabilities: Capabilities) -> Self {
        capabilities.engine = MOCK_ENGINE;
        self.capabilities = capabilities;
        self
    }

    /// Requires [`Auth::Password`] with this password.
    #[must_use]
    pub fn with_password(mut self, password: Secret) -> Self {
        self.password = Some(Arc::new(password));
        self
    }

    /// Shared counters.
    #[must_use]
    pub fn stats(&self) -> Arc<MockStats> {
        Arc::clone(&self.stats)
    }

    /// TLS of the simulated [`MOCK_SELF_SIGNED_HOST`]: a certificate for that name, signed by
    /// a CA trusted only through a CA file named [`MOCK_TRUSTED_CA_FILE`].
    fn simulate_tls(params: &ConnectParams) -> DriverResult<()> {
        let mode = params.tls.mode;
        if !matches!(mode, TlsMode::VerifyCa | TlsMode::VerifyFull) {
            return Ok(());
        }
        let trusted = params
            .tls
            .ca_file
            .as_deref()
            .and_then(std::path::Path::file_name)
            .is_some_and(|n| n == MOCK_TRUSTED_CA_FILE);
        if !trusted {
            return Err(DriverError::Tls(
                "invalid peer certificate: UnknownIssuer".into(),
            ));
        }
        let name = params
            .tls
            .server_name
            .as_deref()
            .unwrap_or(MOCK_SELF_SIGNED_HOST);
        if mode == TlsMode::VerifyFull && name != MOCK_SELF_SIGNED_HOST {
            return Err(DriverError::Tls(
                "invalid peer certificate: NotValidForName".into(),
            ));
        }
        Ok(())
    }

    fn check_engine(engine: &EngineId) -> DriverResult<()> {
        if *engine == MOCK_ENGINE {
            Ok(())
        } else {
            Err(DriverError::InvalidParams(format!(
                "engine `{engine}` is not served by the mock driver"
            )))
        }
    }
}

#[async_trait::async_trait]
impl Driver for MockDriver {
    fn family(&self) -> &'static str {
        "mock"
    }

    fn engines(&self) -> &[EngineId] {
        &MOCK_ENGINES
    }

    fn capabilities(
        &self,
        engine: &EngineId,
        server_version: Option<&str>,
    ) -> DriverResult<Capabilities> {
        Self::check_engine(engine)?;
        let mut c = self.capabilities.clone();
        c.server_version = server_version.map(str::to_owned);
        Ok(c)
    }

    fn dialect(&self, engine: &EngineId) -> DriverResult<Arc<dyn Dialect>> {
        Self::check_engine(engine)?;
        Ok(Arc::new(AnsiDialect))
    }

    async fn connect(&self, params: ConnectParams) -> DriverResult<Box<dyn Session>> {
        Self::check_engine(&params.engine)?;
        self.capabilities.check_properties(&params.properties)?;
        self.capabilities.connect.check_connect_params(&params)?;
        if let Endpoint::Tcp { host, .. } = &params.endpoint
            && host == MOCK_SELF_SIGNED_HOST
        {
            Self::simulate_tls(&params)?;
        }
        if let Some(expected) = &self.password {
            match &params.auth {
                Auth::Password {
                    user,
                    password: Some(given),
                } if given.expose() == expected.expose() => {
                    tracing::debug!(user = %user, "mock: authenticated");
                }
                Auth::Password { user, .. } => {
                    return Err(DriverError::Auth(format!(
                        "password authentication failed for user \"{user}\""
                    )));
                }
                _ => return Err(DriverError::Auth("password required".into())),
            }
        }
        // `params` (and the secret inside) is dropped at the end of this function.
        let mut capabilities = self.capabilities.clone();
        capabilities.server_version = Some("mock 1.0".to_owned());
        let mut session = MockSession {
            server: Arc::new(ServerState::default()),
            tracker: ExecutionTracker::new(),
            stats: Arc::clone(&self.stats),
            capabilities,
            auto_commit: true,
            in_transaction: false,
            read_only: false,
            closed: false,
        };
        if params.read_only {
            session.set_read_only(true).await?;
        }
        if !params.auto_commit {
            session.set_auto_commit(false).await?;
        }
        for init in params.init_statements {
            let mut stream = session
                .execute(Statement::sql(init), ExecOptions::default())
                .await?;
            while let Some(item) = stream.next().await {
                item?;
            }
        }
        self.stats.connects.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(session))
    }
}

/// Per-session state of the simulated server.
#[derive(Debug, Default)]
struct ServerState {
    /// Engine-side read-only setting.
    read_only: AtomicBool,
    /// A cancel request reached the backend; it stops whatever runs (like PostgreSQL).
    cancel_requested: Arc<AtomicBool>,
}

/// The execution's ownership of the session, held by the raw stream. The simulated server
/// confirms the end as soon as the stream ends or is dropped (portal closed).
struct BusyGuard(Option<ExecutionGuard>);

impl Drop for BusyGuard {
    fn drop(&mut self) {
        if let Some(g) = self.0.take() {
            g.complete();
        }
    }
}

/// The simulated server's (unscoped) cancel: like a PostgreSQL cancel request, it hits
/// whatever the session runs. The session scopes it with its [`ExecutionTracker`].
struct MockCancel {
    stats: Arc<MockStats>,
    flag: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl CancelHandle for MockCancel {
    async fn cancel(&self) -> DriverResult<()> {
        self.stats.server_cancels.fetch_add(1, Ordering::SeqCst);
        self.flag.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn confirms_end(&self) -> bool {
        true
    }
}

/// The error the simulated server sends when it stops a cancelled statement.
fn server_cancelled() -> DriverError {
    DriverError::Query {
        code: Some("57014".into()),
        message: "canceling statement due to user request".into(),
        position: None,
    }
}

/// Session of the [`MockDriver`].
#[derive(Debug)]
pub struct MockSession {
    server: Arc<ServerState>,
    tracker: ExecutionTracker,
    stats: Arc<MockStats>,
    capabilities: Capabilities,
    auto_commit: bool,
    in_transaction: bool,
    read_only: bool,
    /// Closed after a detected access-mode change (or by the user).
    closed: bool,
}

fn numbers_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::new("label", DataType::Utf8, false),
    ]))
}

/// Builds rows `from..` until `total` or the batch limits are reached. `label_len` = `None`
/// gives `row-00000042` labels; `Some(n)` gives `n`-byte labels, cut at the value budget (the
/// simulated server never materializes more than the budget).
fn numbers_batch(
    schema: &SchemaRef,
    from: u64,
    total: u64,
    label_len: Option<u64>,
    budget: &FetchBudget,
) -> DriverResult<RecordBatch> {
    let max_value = budget.effective_max_value_bytes();
    let mut limiter = BatchLimiter::new(budget);
    let mut ids = Vec::new();
    let mut labels = Vec::new();
    let mut truncated = false;
    let mut i = from;
    while i < total && !limiter.is_full() {
        // Per-row bound (F5): the id's 8 bytes, then the label within what is left.
        let mut row = RowBudget::new(budget);
        row.add_fixed(8);
        let label = match label_len {
            None => {
                let full = format!("row-{i:08}");
                let keep = row.allow(full.len() as u64);
                truncate_str(&full, keep).0.to_owned()
            }
            Some(n) => {
                // The simulated server never materializes more than the budget allows.
                let keep = row.allow(n).min(max_value);
                "x".repeat(usize::try_from(keep).unwrap_or(0))
            }
        };
        truncated |= row.was_truncated();
        limiter.add_row(row.used() + 4);
        ids.push(i64::try_from(i).unwrap_or(i64::MAX));
        labels.push(label);
        i += 1;
    }
    let schema = if truncated {
        let fields: Vec<Field> = schema
            .fields()
            .iter()
            .map(|f| {
                if f.name() == "label" {
                    f.as_ref().clone().with_metadata([(
                        TRUNCATED_VALUES_METADATA_KEY.to_owned(),
                        "true".to_owned(),
                    )])
                } else {
                    f.as_ref().clone()
                }
            })
            .collect();
        Arc::new(Schema::new(fields))
    } else {
        Arc::clone(schema)
    };
    let cols: Vec<ArrayRef> = vec![
        Arc::new(Int64Array::from(ids)),
        Arc::new(StringArray::from(labels)),
    ];
    RecordBatch::try_new(schema, cols).map_err(|e| DriverError::Protocol(e.to_string()))
}

/// Document `i` of the mock collection; with `payload`, a string field of that many bytes cut
/// at the value / row budget.
fn mock_document(i: u64, payload: Option<u64>, budget: &FetchBudget) -> Document {
    let mut row = RowBudget::new(budget);
    row.add_fixed(8);
    let mut fields = vec![
        (
            "_id".to_owned(),
            DocValue::Int(i64::try_from(i).unwrap_or(0)),
        ),
        ("name".to_owned(), DocValue::String(format!("doc-{i:08}"))),
        (
            "tags".to_owned(),
            DocValue::Array(vec![DocValue::String("nine".to_owned())]),
        ),
    ];
    if let Some(n) = payload {
        let keep = row.allow(n);
        let value = DocValue::String("y".repeat(usize::try_from(keep).unwrap_or(0)));
        let value = if keep < n {
            DocValue::truncated(value, Some(n))
        } else {
            value
        };
        fields.push(("payload".to_owned(), value));
    }
    Document::new(fields)
}

/// `SELECT 1 AS <identifier>`: the identifier, ANSI-unquoted (`"a""b"` → `a"b`; a bare name is
/// lower-cased). `None` for any other statement or a malformed identifier.
fn alias_query(text: &str) -> Option<String> {
    let t = text.trim().trim_end_matches(';').trim_end();
    let prefix = "select 1 as ";
    if t.len() < prefix.len() || !t[..prefix.len()].eq_ignore_ascii_case(prefix) {
        return None;
    }
    let rest = t[prefix.len()..].trim();
    if let Some(inner) = rest.strip_prefix('"').and_then(|r| r.strip_suffix('"')) {
        if inner.replace("\"\"", "").contains('"') {
            return None;
        }
        return Some(inner.replace("\"\"", "\""));
    }
    (!rest.is_empty() && rest.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
        .then(|| rest.to_ascii_lowercase())
}

/// Parses the integer arguments of `name(a, b…)` in `text` (lower-cased).
fn call_args(text: &str, name: &str) -> Option<Vec<u64>> {
    let start = text.find(&format!("{name}("))? + name.len() + 1;
    let end = start + text[start..].find(')')?;
    let inner = text[start..end].trim();
    if inner.is_empty() {
        return Some(Vec::new());
    }
    inner.split(',').map(|a| a.trim().parse().ok()).collect()
}

impl MockSession {
    fn tabular_stream(
        &self,
        total: u64,
        delay: Option<Duration>,
        label_len: Option<u64>,
        budget: FetchBudget,
        guard: BusyGuard,
    ) -> RawResultStream {
        let schema = numbers_schema();
        let stats = Arc::clone(&self.stats);
        let cancelled = Arc::clone(&self.server.cancel_requested);
        let header = ResultItem::ResultSet(ResultSetHeader {
            index: 0,
            kind: ResultSetKind::Tabular(Arc::clone(&schema)),
        });
        let rows = futures_util::stream::unfold((0u64, guard), move |(next, guard)| {
            let schema = Arc::clone(&schema);
            let stats = Arc::clone(&stats);
            let cancelled = Arc::clone(&cancelled);
            async move {
                if next >= total {
                    return None;
                }
                // Idle cursor flagged by a cancel: close it instead of fetching (the server
                // would ignore the cancel while idle).
                if cancelled.load(Ordering::SeqCst) {
                    stats
                        .cursors_closed_on_cancel
                        .fetch_add(1, Ordering::SeqCst);
                    return Some((Err(server_cancelled()), (total, guard)));
                }
                if let Some(d) = delay {
                    tokio::time::sleep(d).await;
                }
                // A cancel that arrived while this batch was being fetched: the server stops
                // at the end of the batch and confirms with an error; the stream then ends and
                // the guard frees the session.
                if cancelled.load(Ordering::SeqCst) {
                    return Some((Err(server_cancelled()), (total, guard)));
                }
                // Rows are produced only when the consumer polls: nothing runs ahead, and a
                // batch ends at the budget's row or byte limit.
                match numbers_batch(&schema, next, total, label_len, &budget) {
                    Ok(batch) => {
                        let count = batch.num_rows() as u64;
                        stats.rows_generated.fetch_add(count, Ordering::SeqCst);
                        Some((Ok(ResultItem::Rows(batch)), (next + count.max(1), guard)))
                    }
                    Err(e) => Some((Err(e), (total, guard))),
                }
            }
        });
        Box::pin(futures_util::StreamExt::chain(
            futures_util::stream::iter([Ok(header)]),
            rows,
        ))
    }

    fn documents_stream(
        &self,
        total: u64,
        payload: Option<u64>,
        budget: FetchBudget,
        guard: BusyGuard,
    ) -> RawResultStream {
        let stats = Arc::clone(&self.stats);
        let cancelled = Arc::clone(&self.server.cancel_requested);
        let header = ResultItem::ResultSet(ResultSetHeader {
            index: 0,
            kind: ResultSetKind::Documents,
        });
        let docs = futures_util::stream::unfold((0u64, guard), move |(next, guard)| {
            let stats = Arc::clone(&stats);
            let cancelled = Arc::clone(&cancelled);
            async move {
                if next >= total {
                    return None;
                }
                if cancelled.load(Ordering::SeqCst) {
                    stats
                        .cursors_closed_on_cancel
                        .fetch_add(1, Ordering::SeqCst);
                    return Some((Err(server_cancelled()), (total, guard)));
                }
                let mut limiter = BatchLimiter::new(&budget);
                let mut docs = Vec::new();
                let mut i = next;
                while i < total && !limiter.is_full() {
                    let doc = mock_document(i, payload, &budget);
                    limiter.add_row(doc.estimated_size());
                    docs.push(doc);
                    i += 1;
                }
                stats.rows_generated.fetch_add(i - next, Ordering::SeqCst);
                Some((Ok(ResultItem::Documents(docs)), (i, guard)))
            }
        });
        Box::pin(futures_util::StreamExt::chain(
            futures_util::stream::iter([Ok(header)]),
            docs,
        ))
    }

    fn multi_stream(guard: BusyGuard) -> DriverResult<RawResultStream> {
        let schema = numbers_schema();
        let items = vec![
            Ok(ResultItem::ResultSet(ResultSetHeader {
                index: 0,
                kind: ResultSetKind::Tabular(Arc::clone(&schema)),
            })),
            Ok(ResultItem::Rows(numbers_batch(
                &schema,
                0,
                1,
                None,
                &FetchBudget::default(),
            )?)),
            Ok(ResultItem::Notice(Notice::new(
                NoticeSeverity::Info,
                "second result set follows",
            ))),
            Ok(ResultItem::ResultSet(ResultSetHeader {
                index: 1,
                kind: ResultSetKind::Tabular(Arc::clone(&schema)),
            })),
            Ok(ResultItem::Rows(numbers_batch(
                &schema,
                1,
                3,
                None,
                &FetchBudget::default(),
            )?)),
        ];
        Ok(Box::pin(futures_util::StreamExt::chain(
            futures_util::stream::iter(items),
            futures_util::stream::once(async move {
                drop(guard);
                Ok(ResultItem::UpdateCount(None))
            }),
        )))
    }

    /// Starts an execution: the session is busy until the stream's guard confirms the end.
    fn start_execution(
        &self,
    ) -> DriverResult<(BusyGuard, Arc<dyn CancelHandle>, crate::ExecutionId)> {
        let guard = self.tracker.begin()?;
        let id = guard.id();
        // A new statement starts with a clean backend cancel flag (no cancel can be in flight:
        // the tracker refuses `begin` while one is).
        self.server.cancel_requested.store(false, Ordering::SeqCst);
        let canceller = self.tracker.cancel_handle(id, self.engine_cancel());
        Ok((BusyGuard(Some(guard)), canceller, id))
    }

    fn engine_cancel(&self) -> Arc<dyn CancelHandle> {
        Arc::new(MockCancel {
            stats: Arc::clone(&self.stats),
            flag: Arc::clone(&self.server.cancel_requested),
        })
    }

    fn single(&mut self, item: DriverResult<ResultItem>, options: &ExecOptions) -> ResultStream {
        ResultStream::new(
            Box::pin(futures_util::stream::iter([item])),
            options,
            self.cancel_handle(),
        )
    }
}

#[async_trait::async_trait]
impl Session for MockSession {
    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    fn server_version(&self) -> Option<&str> {
        self.capabilities.server_version.as_deref()
    }

    fn dialect(&self) -> Arc<dyn Dialect> {
        Arc::new(AnsiDialect)
    }

    fn metadata(&self) -> Arc<dyn Metadata> {
        Arc::new(MockMetadata)
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
        if !statement.params.is_empty() && !self.capabilities.parameters {
            return Err(DriverError::Unsupported(Feature::Parameters));
        }
        if self.closed {
            return Err(DriverError::Closed("session closed".into()));
        }
        if self.tracker.is_busy() {
            return Err(DriverError::Busy);
        }
        // One statement per call: what runs is what was classified (H1).
        ensure_single_statement(&AnsiDialect, &statement.text)?;
        let class = AnsiDialect.classify(&statement.text);
        // Client-side refusal (always on in read-only sessions, I3).
        if self.read_only && class.is_write() {
            return Err(DriverError::ReadOnly(
                "statement refused: the session is read-only".into(),
            ));
        }
        let lower = statement.text.trim().to_ascii_lowercase();
        let first = lower.split_whitespace().next().unwrap_or("").to_owned();

        // Simulated server-side read-only (SessionSetting mechanism): rejects any write,
        // including `mock_write()`, a write the client-side classifier cannot see.
        let engine_side = self.capabilities.read_only.is_engine_side();
        let server_sees_write = matches!(
            first.as_str(),
            "insert" | "update" | "delete" | "merge" | "create" | "drop" | "alter" | "truncate"
        ) || lower.contains("mock_write(");
        if engine_side && self.server.read_only.load(Ordering::SeqCst) && server_sees_write {
            self.stats
                .server_rejected_writes
                .fetch_add(1, Ordering::SeqCst);
            return Err(DriverError::ReadOnly(
                "cannot execute statement in a read-only transaction".into(),
            ));
        }

        // `mock_lift_read_only()` simulates a statement that lifts the engine-side setting
        // without being classified as such. For liftable mechanisms the driver verifies the
        // access mode after every statement and closes the session if it changed (H2).
        if engine_side && lower.contains("mock_lift_read_only(") {
            self.server.read_only.store(false, Ordering::SeqCst);
        }
        if self.read_only
            && self.capabilities.read_only.is_liftable()
            && !self.server.read_only.load(Ordering::SeqCst)
        {
            self.closed = true;
            tracing::warn!("mock: read-only mode was lifted by a statement; session closed");
            return Err(DriverError::ReadOnly(
                "the session left read-only mode; it was closed".into(),
            ));
        }

        match class {
            StatementClass::Read => {
                let budget = options.budget;
                if let Some(name) = alias_query(&statement.text) {
                    let schema =
                        Arc::new(Schema::new(vec![Field::new(name, DataType::Int64, false)]));
                    let batch = RecordBatch::try_new(
                        Arc::clone(&schema),
                        vec![Arc::new(Int64Array::from(vec![1_i64]))],
                    )
                    .map_err(|e| DriverError::Protocol(e.to_string()))?;
                    let items = vec![
                        Ok(ResultItem::ResultSet(ResultSetHeader::tabular(0, schema))),
                        Ok(ResultItem::Rows(batch)),
                    ];
                    return Ok(ResultStream::from_items(items, &options));
                }
                if let Some(args) = call_args(&lower, "slow_numbers") {
                    let [n, ms] = args[..] else {
                        return Err(DriverError::Query {
                            code: Some("42883".into()),
                            message: "slow_numbers(rows, delay_ms) expects two arguments".into(),
                            position: None,
                        });
                    };
                    let (guard, canceller, id) = self.start_execution()?;
                    let delay = Some(Duration::from_millis(ms));
                    let raw = self.tabular_stream(n, delay, None, budget, guard);
                    return Ok(ResultStream::new(raw, &options, canceller).with_execution(id));
                }
                if let Some(args) = call_args(&lower, "wide_documents") {
                    let [n, len] = args[..] else {
                        return Err(DriverError::Query {
                            code: Some("42883".into()),
                            message: "wide_documents(docs, value_bytes) expects two arguments"
                                .into(),
                            position: None,
                        });
                    };
                    let (guard, canceller, id) = self.start_execution()?;
                    let raw = self.documents_stream(n, Some(len), budget, guard);
                    return Ok(ResultStream::new(raw, &options, canceller).with_execution(id));
                }
                if let Some(args) = call_args(&lower, "numbers") {
                    let n = args.first().copied().unwrap_or(0);
                    let (guard, canceller, id) = self.start_execution()?;
                    let raw = self.tabular_stream(n, None, None, budget, guard);
                    return Ok(ResultStream::new(raw, &options, canceller).with_execution(id));
                }
                if let Some(args) = call_args(&lower, "wide") {
                    let [n, len] = args[..] else {
                        return Err(DriverError::Query {
                            code: Some("42883".into()),
                            message: "wide(rows, value_bytes) expects two arguments".into(),
                            position: None,
                        });
                    };
                    let (guard, canceller, id) = self.start_execution()?;
                    let raw = self.tabular_stream(n, None, Some(len), budget, guard);
                    return Ok(ResultStream::new(raw, &options, canceller).with_execution(id));
                }
                if lower.contains("mock_write(") {
                    return Ok(self.single(Ok(ResultItem::UpdateCount(Some(1))), &options));
                }
                if let Some(args) = call_args(&lower, "notices") {
                    let n = args.first().copied().unwrap_or(0);
                    let items: Vec<_> = (0..n)
                        .map(|i| {
                            Ok(ResultItem::Notice(Notice::new(
                                NoticeSeverity::Info,
                                format!("notice {i}"),
                            )))
                        })
                        .collect();
                    return Ok(ResultStream::new(
                        Box::pin(futures_util::stream::iter(items)),
                        &options,
                        self.cancel_handle(),
                    ));
                }
                if let Some(args) = call_args(&lower, "documents") {
                    let n = args.first().copied().unwrap_or(0);
                    let (guard, canceller, id) = self.start_execution()?;
                    let raw = self.documents_stream(n, None, budget, guard);
                    return Ok(ResultStream::new(raw, &options, canceller).with_execution(id));
                }
                if call_args(&lower, "multi").is_some() {
                    let (guard, canceller, id) = self.start_execution()?;
                    let raw = Self::multi_stream(guard)?;
                    return Ok(ResultStream::new(raw, &options, canceller).with_execution(id));
                }
                if lower.contains("error(") {
                    return Ok(self.single(
                        Err(DriverError::Query {
                            code: Some("XX000".into()),
                            message: "mock error requested".into(),
                            position: None,
                        }),
                        &options,
                    ));
                }
                Err(DriverError::Query {
                    code: Some("42P01".into()),
                    message: "mock: unknown relation".into(),
                    position: None,
                })
            }
            StatementClass::Transaction => {
                match first.as_str() {
                    "begin" | "start" => self.begin().await?,
                    "commit" | "end" => self.commit().await?,
                    "rollback" => self.rollback().await?,
                    _ => {}
                }
                Ok(self.single(Ok(ResultItem::UpdateCount(None)), &options))
            }
            StatementClass::Session | StatementClass::ChangesAccessMode => {
                Ok(self.single(Ok(ResultItem::UpdateCount(None)), &options))
            }
            StatementClass::Write | StatementClass::Ddl | StatementClass::Destructive => {
                if !self.auto_commit {
                    self.in_transaction = true;
                }
                Ok(self.single(Ok(ResultItem::UpdateCount(Some(1))), &options))
            }
            StatementClass::Unknown => Err(DriverError::Query {
                code: Some("42601".into()),
                message: "mock: syntax error".into(),
                position: Some(1),
            }),
        }
    }

    fn cancel_handle(&self) -> Arc<dyn CancelHandle> {
        // Scoped to the execution running now; a no-op when idle or once it ended (M6).
        self.tracker.current_cancel_handle(self.engine_cancel())
    }

    fn auto_commit(&self) -> bool {
        self.auto_commit
    }

    fn in_transaction(&self) -> bool {
        self.in_transaction
    }

    async fn set_auto_commit(&mut self, auto_commit: bool) -> DriverResult<()> {
        if !auto_commit && !self.capabilities.transactions.manual_commit {
            return Err(DriverError::Unsupported(Feature::Transactions));
        }
        if auto_commit {
            self.in_transaction = false;
        }
        self.auto_commit = auto_commit;
        Ok(())
    }

    async fn begin(&mut self) -> DriverResult<()> {
        if !self.capabilities.transactions.manual_commit {
            return Err(DriverError::Unsupported(Feature::Transactions));
        }
        self.in_transaction = true;
        Ok(())
    }

    async fn commit(&mut self) -> DriverResult<()> {
        if !self.capabilities.transactions.manual_commit {
            return Err(DriverError::Unsupported(Feature::Transactions));
        }
        self.in_transaction = false;
        Ok(())
    }

    async fn rollback(&mut self) -> DriverResult<()> {
        if !self.capabilities.transactions.manual_commit {
            return Err(DriverError::Unsupported(Feature::Transactions));
        }
        self.in_transaction = false;
        Ok(())
    }

    async fn savepoint(&mut self, _name: &str) -> DriverResult<()> {
        if !self.capabilities.transactions.savepoints {
            return Err(DriverError::Unsupported(Feature::Savepoints));
        }
        Ok(())
    }

    async fn rollback_to_savepoint(&mut self, _name: &str) -> DriverResult<()> {
        if !self.capabilities.transactions.savepoints {
            return Err(DriverError::Unsupported(Feature::Savepoints));
        }
        Ok(())
    }

    fn is_read_only(&self) -> bool {
        self.read_only
    }

    async fn set_read_only(&mut self, read_only: bool) -> DriverResult<()> {
        if self.capabilities.read_only.is_engine_side() {
            // Simulated `SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY`.
            self.server.read_only.store(read_only, Ordering::SeqCst);
        }
        self.read_only = read_only;
        Ok(())
    }

    async fn ping(&mut self) -> DriverResult<()> {
        if self.closed {
            return Err(DriverError::Closed("session closed".into()));
        }
        Ok(())
    }

    async fn close(self: Box<Self>) -> DriverResult<()> {
        Ok(())
    }
}

/// Metadata of the mock: catalog `nine` → schema `public` → tables `t000`… → columns.
#[derive(Debug, Clone, Copy)]
struct MockMetadata;

impl MockMetadata {
    fn objects(parent: &ObjectPath, kind: ObjectKind) -> DriverResult<Vec<ObjectInfo>> {
        let plain = |path: ObjectPath| ObjectInfo {
            path,
            comment: None,
            details: ObjectDetails::None,
        };
        Ok(match (parent.kind(), kind) {
            (None, ObjectKind::Catalog) => vec![plain(parent.child(kind, "nine"))],
            (Some(ObjectKind::Catalog), ObjectKind::Schema) => {
                vec![plain(parent.child(kind, "public"))]
            }
            (Some(ObjectKind::Schema), ObjectKind::Table) => (0..MOCK_TABLE_COUNT)
                .map(|i| ObjectInfo {
                    path: parent.child(kind, format!("t{i:03}")),
                    comment: None,
                    details: ObjectDetails::Table(TableInfo {
                        estimated_rows: Some(1000),
                        flavour: None,
                        partition_key: None,
                    }),
                })
                .collect(),
            (Some(ObjectKind::Table), ObjectKind::Column) => {
                [("id", "bigint", false), ("label", "text", true)]
                    .iter()
                    .zip(1u32..)
                    .map(|((name, ty, nullable), ordinal)| ObjectInfo {
                        path: parent.child(kind, *name),
                        comment: None,
                        details: ObjectDetails::Column(ColumnInfo {
                            ordinal,
                            data_type: (*ty).to_owned(),
                            nullable: *nullable,
                            default: None,
                            primary_key: ordinal == 1,
                            auto_generated: ordinal == 1,
                        }),
                    })
                    .collect()
            }
            _ => return Err(DriverError::Unsupported(Feature::Metadata(kind))),
        })
    }
}

#[async_trait::async_trait]
impl Metadata for MockMetadata {
    fn child_kinds(&self, parent: Option<ObjectKind>) -> Vec<ObjectKind> {
        match parent {
            None => vec![ObjectKind::Catalog],
            Some(ObjectKind::Catalog) => vec![ObjectKind::Schema],
            Some(ObjectKind::Schema) => vec![ObjectKind::Table],
            Some(ObjectKind::Table) => vec![ObjectKind::Column],
            Some(_) => Vec::new(),
        }
    }

    async fn list(
        &self,
        parent: &ObjectPath,
        kind: ObjectKind,
        page: PageRequest,
    ) -> DriverResult<Page<ObjectInfo>> {
        let mut all = Self::objects(parent, kind)?;
        if let Some(f) = &page.filter {
            all.retain(|o| o.name().contains(f.as_str()));
        }
        let offset: usize = match &page.cursor {
            None => 0,
            Some(c) => c
                .parse()
                .map_err(|_| DriverError::InvalidParams("bad page cursor".into()))?,
        };
        let limit = usize::try_from(page.limit.max(1)).unwrap_or(usize::MAX);
        let end = offset.saturating_add(limit).min(all.len());
        let items = all
            .get(offset..end)
            .map(<[ObjectInfo]>::to_vec)
            .unwrap_or_default();
        let next_cursor = (end < all.len()).then(|| end.to_string());
        Ok(Page { items, next_cursor })
    }

    async fn ddl(&self, object: &ObjectPath) -> DriverResult<String> {
        match object.leaf() {
            Some(seg) if seg.kind == ObjectKind::Table => Ok(format!(
                "CREATE TABLE {} (\n  id bigint PRIMARY KEY,\n  label text\n);",
                AnsiDialect.quote_identifier(&seg.name)
            )),
            Some(seg) => Err(DriverError::Unsupported(Feature::Metadata(seg.kind))),
            None => Err(DriverError::InvalidParams("no object".into())),
        }
    }
}
