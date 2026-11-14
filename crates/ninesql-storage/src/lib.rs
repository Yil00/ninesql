//! Workspace storage: XDG paths, versioned JSON / TOML files, atomic writes, private permissions
//! and forward migrations (ADR-0008). ROADMAP: P0-D.
//!
//! - [`paths`]: where every NineSQL file lives under the XDG base directories.
//! - [`fs`]: atomic writes (temp file + `fsync` + rename + directory `fsync`), directories `0700`,
//!   files `0600`, bounded reads.
//! - [`document`]: files with a top-level `schema_version`, migrated forward on load with a `.bak`
//!   of the previous file.
//! - [`hygiene`]: detection of secret-looking keys in loaded files (defence in depth for I1).
//!
//! This crate stores whatever model it is given; it never sees secret values (the connection model
//! in `ninesql-core` only holds secret references).
#![forbid(unsafe_code)]

pub mod document;
mod error;
pub mod fs;
pub mod hygiene;
pub mod paths;

pub use document::{Document, Format, Loaded, Migration};
pub use error::StorageError;
pub use paths::{Paths, ProjectName, ProjectPaths, XdgDirs};
