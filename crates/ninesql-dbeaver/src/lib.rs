//! DBeaver interoperability: read-only workspace import (ADR-0006, invariants I1 and I4).
//! ROADMAP: P2-A (this parser), P5-D (export). **Security review required for every change.**
//!
//! # Pipeline
//! 1. [`detect`]: find DBeaver workspaces (native, Flatpak, Snap, or a folder the user picks) and
//!    list their projects.
//! 2. [`model`]: parse each `data-sources*.json` into a tolerant model. Parsing is total: a key
//!    with an unexpected type or an unknown key is kept in an `extra` map, never a panic.
//! 3. [`credentials`]: decrypt `credentials-config.json` (AES-128-CBC with DBeaver Community's
//!    public fixed key) into zeroized buffers. Files that are not in the Community format (DBeaver
//!    PRO master password, corruption) are detected and skipped.
//! 4. [`mapping`]: turn both into an import model mirroring `ninesql-core`'s connection model,
//!    with per-connection issues ([`report`]) and the secrets to move to `ninesql-secrets`.
//!
//! [`import_project`] runs the whole pipeline for one project.
//!
//! # Rules this crate enforces
//! - **I4**: every DBeaver file is opened read-only through one function (`fsread`), never
//!   created, written, renamed, deleted or locked. `.bak` and other dot-files are ignored.
//! - **I1**: decrypted secrets only exist as [`ninesql_secrets::SecretString`] (zeroized on drop,
//!   redacted in `Debug`) inside [`mapping::ImportedSecret`]. The caller (P2-C) hands them to
//!   `ninesql-secrets`; this crate stores nothing. Issues, errors and logs name keys and DBeaver
//!   ids, never values.
//! - **I5**: TLS settings that do not verify the server become explicit, flagged downgrades;
//!   SSH host keys are never trusted on import.
//!
//! # Why a mirror model
//! `ninesql-core` depends on this crate (docs/02-architecture.md), so the import model in
//! [`mapping`] mirrors `ninesql_core::Connection` field by field instead of using it. The core
//! converts it and runs `Connection::validate` / `sanitize` before persisting (P2-C).
#![forbid(unsafe_code)]

mod coverage;
pub mod credentials;
pub mod detect;
mod error;
mod fsread;
mod jdbc;
pub mod mapping;
pub mod model;
pub mod report;

pub use credentials::{Credentials, CredentialsError, decrypt_credentials};
pub use detect::{InstallKind, Project, SearchRoots, Workspace, detect_workspaces, open_folder};
pub use error::DbeaverError;
pub use mapping::{ConnectionImport, ImportedConnection, ImportedSecret, Outcome, ProjectImport};
pub use model::{DataSources, parse_data_sources};
pub use report::{ImportIssue, IssueKind, Severity};

/// Reads, parses, decrypts and maps one DBeaver project. Never fails as a whole: unreadable or
/// invalid files become project-level issues of the result.
///
/// Opens files read-only and never writes anything under the DBeaver directory (I4).
#[must_use = "the import result carries secrets to move and issues to show"]
pub fn import_project(project: &Project) -> ProjectImport {
    mapping::import_project(project)
}
