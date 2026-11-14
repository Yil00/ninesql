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
//! - [`ResultStream`]: Arrow [`RecordBatch`](arrow_array::RecordBatch)es, [`Document`]s, update
//!   counts and notices, with the memory budget, timeout and cancel enforced uniformly (I6);
//!   ends with [`ResultItem::Truncated`] when the budget is reached.
//! - [`Capabilities`]: plain data the UI reads to offer only what the engine supports (I8);
//!   mirrored in `docs/08-engine-capabilities.md`.
//! - [`DriverError`]: typed, user-presentable errors that never carry secrets or values.
#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod capabilities;
pub mod dialect;
pub mod driver;
pub mod error;
pub mod metadata;
pub mod params;
pub mod result;
pub mod secret;
pub mod statement;

pub use capabilities::{
    CancelSupport, Capabilities, EditingCapabilities, ExplainFormat, Feature, IsolationLevel,
    MetadataCapabilities, ObjectKind, QueryLanguage, ReadOnlyMechanism, ResultKind,
    TransactionCapabilities,
};
pub use dialect::{
    AnsiDialect, Dialect, LexicalRules, RowChange, RowEdit, StatementClass, StatementSpan,
};
pub use driver::{Driver, Session};
pub use error::{DriverError, DriverResult, redact_connection_string, redact_secret};
pub use metadata::{
    ColumnInfo, Metadata, ObjectDetails, ObjectInfo, ObjectPath, Page, PageRequest, PathSegment,
    TableInfo,
};
pub use params::{
    Auth, ClientCertificate, ConnectParams, Endpoint, EngineId, HostKeyVerification,
    SecurityWarning, TlsConfig, TlsMode, TunnelInfo,
};
pub use result::{
    CancelHandle, DocValue, Document, ENGINE_TYPE_METADATA_KEY, NoServerCancel, Notice,
    NoticeSeverity, RawResultStream, ResultItem, ResultSetHeader, ResultSetKind, ResultStream,
    Truncation, TruncationReason,
};
pub use secret::Secret;
pub use statement::{CancelToken, ExecOptions, FetchBudget, Param, Statement};

/// Re-exported Arrow array crate, so drivers and the grid use the exact same version.
pub use arrow_array;
/// Re-exported Arrow schema crate.
pub use arrow_schema;
