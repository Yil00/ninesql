//! NineSQL driver API (ADR-0004): the contract between the core / UI and every driver family.
//!
//! ROADMAP: P0-B. This crate is the single synchronization point between UI and drivers; once
//! frozen, every change goes through a reviewed PR with a `security-reviewer` review.
//!
//! # Overview
//! - [`Driver`]: one family (pgwire, mysqlwire…) serving several engines ([`EngineId`]);
//!   reports [`Capabilities`] and a [`Dialect`] per engine and opens [`Session`]s.
//! - [`ConnectParams`]: fully resolved parameters with secure defaults (TLS `verify-full`,
//!   strict SSH host keys, I5) and secrets as zeroized [`Secret`]s (I1).
//! - [`Session`]: executes [`Statement`]s into a [`ResultStream`], cancels, controls
//!   transactions and the engine-side read-only mode (I3), exposes [`Metadata`].
//! - [`ExecutionTracker`]: execution ids, the busy state and execution-scoped cancel handles,
//!   so a late cancel never reaches the next statement.
//! - [`ResultStream`]: Arrow [`RecordBatch`](arrow_array::RecordBatch)es, [`Document`]s, update
//!   counts and notices, with the memory budget, timeout and cancel enforced uniformly (I6);
//!   ends with [`ResultItem::Truncated`] when the budget is reached.
//! - [`Capabilities`]: plain data the UI reads to offer only what the engine supports (I8);
//!   mirrored in `docs/08-engine-capabilities.md`.
//! - [`DriverError`]: typed, user-presentable errors that never carry secrets or values.
//!
//! With the `mock` feature, `mock` provides an in-memory driver for UI and core tests.
#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod capabilities;
#[cfg(feature = "mock")]
pub mod conformance;
pub mod dialect;
pub mod driver;
pub mod error;
pub mod execution;
pub mod metadata;
#[cfg(feature = "mock")]
pub mod mock;
pub mod params;
pub mod result;
pub mod secret;
pub mod statement;

pub use capabilities::{
    CancelSupport, Capabilities, ConnectCapabilities, EditingCapabilities, ExplainFormat, Feature,
    IsolationLevel, MetadataCapabilities, ObjectKind, QueryLanguage, ReadOnlyMechanism, ResultKind,
    TransactionCapabilities,
};
pub use dialect::{
    AnsiDialect, Dialect, LexicalRules, RowChange, RowEdit, StatementClass, StatementSpan,
    ensure_single_statement,
};
pub use driver::{Driver, Session};
pub use error::{DriverError, DriverResult, redact_connection_string, redact_secret};
pub use execution::{ExecutionGuard, ExecutionId, ExecutionTracker};
pub use metadata::{
    ColumnInfo, Metadata, ObjectDetails, ObjectInfo, ObjectPath, Page, PageRequest, PathSegment,
    TableInfo, escape_like,
};
pub use params::{
    Auth, AuthKind, ClientCertificate, ConnectParams, Endpoint, EngineId, HostKeyVerification,
    PasswordExchange, PropertyValue, ProxyInfo, ProxyKind, SecurityWarning, SshHop, TlsConfig,
    TlsMode, TunnelInfo, check_properties, echo_property_key, is_reserved_property,
};
pub use result::{
    CancelHandle, DocValue, Document, ENGINE_TYPE_METADATA_KEY, MAX_DOCUMENT_DEPTH, NoServerCancel,
    Notice, NoticeSeverity, RawResultStream, ResultItem, ResultSetHeader, ResultSetKind,
    ResultStream, SERVER_CANCEL_GRACE, TRUNCATED_VALUES_METADATA_KEY, Truncation, TruncationReason,
};
pub use secret::Secret;
pub use statement::{
    BatchLimiter, CancelToken, ExecOptions, FetchBudget, Param, RowBudget, Statement,
    truncate_bytes, truncate_str,
};

/// Re-exported Arrow array crate (Arrow 60), so drivers and the grid use the exact same version.
pub use arrow_array;
/// Re-exported Arrow schema crate (Arrow 60).
///
/// Since Arrow 60, field and schema metadata is an [`arrow_schema::Metadata`] map rather than a
/// `HashMap<String, String>`: read the `ninesql.*` keys with `field.metadata().get(key)` and set
/// them with `Field::with_metadata([(key, value)])`.
pub use arrow_schema;
