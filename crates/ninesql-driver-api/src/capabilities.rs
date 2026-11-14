//! Capability model (ADR-0004, invariant I8).
//!
//! A [`Capabilities`] value is plain data: what one engine, at one server version, supports
//! through its driver. The UI reads it to decide what to offer; anything not declared is
//! **hidden or disabled**, never attempted and left to fail. A driver declares a capability only
//! when its integration tests prove it.
//!
//! The human-readable mirror is `docs/08-engine-capabilities.md`; a PR that changes what a driver
//! declares updates that file in the same PR.
//!
//! | Field | Meaning | UI effect when absent |
//! |---|---|---|
//! | [`query_languages`](Capabilities::query_languages) | Languages accepted by `Session::execute` | Editor modes not offered |
//! | [`result_kinds`](Capabilities::result_kinds) | Tabular rows and / or documents | Grid or document viewer not offered |
//! | [`metadata`](Capabilities::metadata) | Object kinds listed in the navigator, kinds with DDL | Navigator nodes / "Generate DDL" hidden |
//! | [`editing`](Capabilities::editing) | In-grid insert / update / delete | Grid is read-only |
//! | [`transactions`](Capabilities::transactions) | Manual commit, savepoints, isolation | Commit / rollback toolbar hidden, auto-commit forced |
//! | [`read_only`](Capabilities::read_only) | How a read-only session is enforced (I3) | `ClientSideOnly` is shown in plain words |
//! | [`cancel`](Capabilities::cancel) | Server-side cancel or local abort only | Cancel still offered; label says "stop fetching" for `LocalOnly` |
//! | [`explain`](Capabilities::explain) | Explain formats | "Explain" hidden |
//! | [`multiple_result_sets`](Capabilities::multiple_result_sets) | One statement may yield several result sets | Single result tab |
//! | [`server_side_timeout`](Capabilities::server_side_timeout) | Engine enforces a statement timeout itself | Timeout enforced by the stream wrapper only |
//! | [`tls`](Capabilities::tls) | TLS modes the engine supports | TLS section disabled |
//! | [`parameters`](Capabilities::parameters) | Bind parameters in statements | Parameter prompt not offered |

use std::collections::BTreeSet;

use crate::params::{EngineId, TlsMode};

/// Everything one engine supports through its driver. See the module documentation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Capabilities {
    /// Engine these capabilities describe.
    pub engine: EngineId,
    /// Server version they were computed for (`None` before connect).
    pub server_version: Option<String>,
    /// Languages accepted by [`Session::execute`](crate::Session::execute).
    pub query_languages: BTreeSet<QueryLanguage>,
    /// Kinds of result the engine produces.
    pub result_kinds: BTreeSet<ResultKind>,
    /// Navigator and DDL support.
    pub metadata: MetadataCapabilities,
    /// In-grid editing.
    pub editing: EditingCapabilities,
    /// Transaction control.
    pub transactions: TransactionCapabilities,
    /// How read-only sessions are enforced (I3, ADR-0007).
    pub read_only: ReadOnlyMechanism,
    /// How a running statement is cancelled.
    pub cancel: CancelSupport,
    /// Explain formats offered.
    pub explain: BTreeSet<ExplainFormat>,
    /// A single statement may return several result sets.
    pub multiple_result_sets: bool,
    /// The engine enforces statement timeouts itself (in addition to the stream wrapper).
    pub server_side_timeout: bool,
    /// TLS modes the engine supports; empty for file engines.
    pub tls: BTreeSet<TlsMode>,
    /// Bind parameters are supported in [`Statement`](crate::Statement)s.
    pub parameters: bool,
}

impl Capabilities {
    /// The most conservative declaration: nothing but plain SQL text returning rows, no
    /// transactions, client-side read-only, local cancel only. Drivers start from this and
    /// switch on what their tests prove.
    #[must_use]
    pub fn minimal(engine: EngineId) -> Self {
        Self {
            engine,
            server_version: None,
            query_languages: BTreeSet::from([QueryLanguage::Sql]),
            result_kinds: BTreeSet::from([ResultKind::Tabular]),
            metadata: MetadataCapabilities::default(),
            editing: EditingCapabilities::default(),
            transactions: TransactionCapabilities::default(),
            read_only: ReadOnlyMechanism::ClientSideOnly,
            cancel: CancelSupport::LocalOnly,
            explain: BTreeSet::new(),
            multiple_result_sets: false,
            server_side_timeout: false,
            tls: BTreeSet::new(),
            parameters: false,
        }
    }

    /// `true` when `feature` is declared.
    #[must_use]
    pub fn supports(&self, feature: Feature) -> bool {
        match feature {
            Feature::Transactions => self.transactions.manual_commit,
            Feature::Savepoints => self.transactions.savepoints,
            Feature::ServerSideCancel => self.cancel == CancelSupport::ServerSide,
            Feature::EngineSideReadOnly => self.read_only.is_engine_side(),
            Feature::Explain => !self.explain.is_empty(),
            Feature::Ddl => !self.metadata.ddl.is_empty(),
            Feature::Editing => self.editing.any(),
            Feature::MultipleResultSets => self.multiple_result_sets,
            Feature::Documents => self.result_kinds.contains(&ResultKind::Documents),
            Feature::Parameters => self.parameters,
            Feature::Metadata(kind) => self.metadata.kinds.contains(&kind),
            Feature::Language(lang) => self.query_languages.contains(&lang),
        }
    }
}

/// A capability that can be checked with [`Capabilities::supports`], and named in
/// [`DriverError::Unsupported`](crate::DriverError::Unsupported).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Feature {
    /// Manual commit / rollback.
    Transactions,
    /// Savepoints inside a transaction.
    Savepoints,
    /// Cancel reaches the server.
    ServerSideCancel,
    /// Read-only enforced by the engine.
    EngineSideReadOnly,
    /// At least one explain format.
    Explain,
    /// DDL generation for at least one object kind.
    Ddl,
    /// In-grid editing.
    Editing,
    /// Several result sets per statement.
    MultipleResultSets,
    /// Document results.
    Documents,
    /// Bind parameters.
    Parameters,
    /// Listing one metadata object kind.
    Metadata(ObjectKind),
    /// One query language.
    Language(QueryLanguage),
}

/// Language of a statement.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum QueryLanguage {
    /// The engine's SQL dialect (also CQL, ClickHouse SQL, Trino SQL, InfluxDB SQL…).
    Sql,
    /// InfluxQL (InfluxDB 3).
    InfluxQl,
    /// MongoDB shell-style commands / aggregation pipelines as JSON.
    MongoQuery,
    /// Redis / Valkey commands.
    RedisCommand,
    /// Elasticsearch / OpenSearch query DSL (JSON) against REST endpoints.
    SearchDsl,
    /// Cypher (Neo4j).
    Cypher,
}

/// Kind of result an engine produces.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ResultKind {
    /// Arrow record batches.
    Tabular,
    /// Document batches (JSON-like trees keeping engine types).
    Documents,
}

/// Database object kinds shown in the navigator. Engines declare the subset they list.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum ObjectKind {
    /// Database / catalog.
    Catalog,
    /// Schema / namespace / keyspace.
    Schema,
    /// Table (including hypertables, which carry a detail flag).
    Table,
    /// View.
    View,
    /// Materialized view.
    MaterializedView,
    /// Column of a table or view.
    Column,
    /// Index.
    Index,
    /// Primary key, unique or check constraint.
    Constraint,
    /// Foreign key.
    ForeignKey,
    /// Trigger.
    Trigger,
    /// Function or procedure.
    Routine,
    /// Sequence / generator.
    Sequence,
    /// User-defined type / domain.
    Type,
    /// Partition of a table.
    Partition,
    /// User.
    User,
    /// Role.
    Role,
    /// Document collection (MongoDB) or search index (Elasticsearch / OpenSearch).
    Collection,
    /// Key in a key-value store (listed by pattern, paged).
    Key,
    /// Graph node label.
    Label,
    /// Graph relationship type.
    RelationshipType,
}

/// Navigator support.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MetadataCapabilities {
    /// Object kinds [`Metadata::list`](crate::Metadata::list) can return.
    pub kinds: BTreeSet<ObjectKind>,
    /// Object kinds [`Metadata::ddl`](crate::Metadata::ddl) can produce DDL for.
    pub ddl: BTreeSet<ObjectKind>,
}

/// In-grid editing support. Editing generates DML through the [`Dialect`](crate::Dialect).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EditingCapabilities {
    /// Rows can be inserted.
    pub insert: bool,
    /// Rows can be updated.
    pub update: bool,
    /// Rows can be deleted.
    pub delete: bool,
    /// Update / delete need a primary or unique key in the result (otherwise the grid stays
    /// read-only for that result).
    pub requires_unique_key: bool,
}

impl EditingCapabilities {
    /// `true` when any edit is possible.
    #[must_use]
    pub fn any(&self) -> bool {
        self.insert || self.update || self.delete
    }
}

/// Transaction control support.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TransactionCapabilities {
    /// `begin` / `commit` / `rollback` and auto-commit off are supported.
    pub manual_commit: bool,
    /// Savepoints are supported.
    pub savepoints: bool,
    /// Isolation levels that can be selected.
    pub isolation_levels: BTreeSet<IsolationLevel>,
}

/// Transaction isolation level.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum IsolationLevel {
    /// Read uncommitted.
    ReadUncommitted,
    /// Read committed.
    ReadCommitted,
    /// Repeatable read.
    RepeatableRead,
    /// Snapshot (engines that name it distinctly).
    Snapshot,
    /// Serializable.
    Serializable,
}

/// How a read-only session is enforced (I3, ADR-0007 §2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ReadOnlyMechanism {
    /// A session setting makes every transaction read-only
    /// (`SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY`, `SET SESSION TRANSACTION READ ONLY`).
    SessionSetting,
    /// Every transaction is started read-only (Firebird `isc_tpb_read`, Neo4j `READ` mode).
    TransactionMode,
    /// The database file is opened read-only (SQLite `SQLITE_OPEN_READ_ONLY` + `query_only`,
    /// DuckDB `access_mode = READ_ONLY`).
    OpenMode,
    /// A per-session / per-query server setting (ClickHouse `readonly = 1`).
    ServerSetting,
    /// The driver only calls read endpoints in read-only mode (InfluxDB 3 query API, search
    /// engines' `_search` / `_sql`).
    ReadEndpointsOnly,
    /// No engine-side mechanism: the driver refuses writes it can classify, which a statement it
    /// fails to classify can bypass. The UI says so in plain words and recommends a read-only
    /// account.
    ClientSideOnly,
}

impl ReadOnlyMechanism {
    /// `true` when the engine itself rejects writes.
    #[must_use]
    pub fn is_engine_side(self) -> bool {
        self != Self::ClientSideOnly
    }
}

/// How a running statement is cancelled.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CancelSupport {
    /// The server stops the statement (cancel request, `KILL QUERY`, interrupt, tasks API…).
    ServerSide,
    /// Only the local fetch stops; the server may keep running the statement until it notices.
    LocalOnly,
}

/// Explain output format.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum ExplainFormat {
    /// Plain text plan.
    Text,
    /// JSON plan.
    Json,
    /// Plan with actual execution statistics (`EXPLAIN ANALYZE`, `PROFILE`). Runs the statement.
    Analyze,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_declares_almost_nothing() {
        let c = Capabilities::minimal(EngineId::QUESTDB);
        assert!(c.supports(Feature::Language(QueryLanguage::Sql)));
        assert!(!c.supports(Feature::Transactions));
        assert!(!c.supports(Feature::EngineSideReadOnly));
        assert!(!c.supports(Feature::ServerSideCancel));
        assert!(!c.supports(Feature::Explain));
        assert!(!c.supports(Feature::Ddl));
        assert!(!c.supports(Feature::Editing));
        assert!(!c.supports(Feature::Documents));
        assert!(!c.supports(Feature::Metadata(ObjectKind::Table)));
    }

    #[test]
    fn engine_side_read_only() {
        assert!(ReadOnlyMechanism::SessionSetting.is_engine_side());
        assert!(ReadOnlyMechanism::OpenMode.is_engine_side());
        assert!(!ReadOnlyMechanism::ClientSideOnly.is_engine_side());
    }
}
