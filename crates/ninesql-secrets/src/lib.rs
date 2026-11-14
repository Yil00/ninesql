//! Secrets: secret references, the [`SecretStore`] trait, an in-memory test store and a
//! Secret Service backend spike (ADR-0005, invariant I1). ROADMAP: P0-D, P1-D.
//!
//! **Security review required for every change to this crate.**
//!
//! Rules enforced here:
//! - Workspace files only ever hold a [`SecretRef`] (`{ "store": "keyring" | "vault", "id": "…" }`),
//!   never a secret value.
//! - Secret values travel as [`SecretString`] (`secrecy`), which is zeroized on drop and never
//!   printed by `Debug`.
//! - Errors never carry a secret value (see [`SecretError`]).
//!
//! Store access is **blocking**. Callers run it on a blocking thread (for example
//! `tokio::task::spawn_blocking`), never on the GPUI foreground thread (I6) and never directly on
//! an async worker thread.
#![forbid(unsafe_code)]

mod error;
#[cfg(any(test, feature = "memory-store"))]
mod memory;
mod reference;
mod registry;
#[cfg(feature = "secret-service")]
mod secret_service;
mod store;

pub use error::SecretError;
#[cfg(any(test, feature = "memory-store"))]
pub use memory::MemoryStore;
pub use reference::{InvalidSecretId, SecretId, SecretKind, SecretRef, StoreKind};
pub use registry::SecretStores;
pub use secrecy::{ExposeSecret, SecretString};
#[cfg(feature = "secret-service")]
pub use secret_service::{InvalidProjectId, SecretServiceStore};
pub use store::{SecretMeta, SecretStore};

/// Service name used for every item NineSQL writes to a secret store (ADR-0005).
pub const SERVICE: &str = "ninesql";
