//! Secrets: secret references, the [`SecretStore`] trait, the encrypted [`Vault`], the
//! Secret Service backend (feature `secret-service`), backend selection, the "ask every time"
//! [`SessionSecretCache`] and an in-memory test store (ADR-0005, invariant I1). ROADMAP: P0-D,
//! P1-D.
//!
//! **Security review required for every change to this crate.**
//!
//! Rules enforced here:
//! - Workspace files only ever hold a [`SecretRef`] (`{ "store": "keyring" | "vault", "id": "…" }`),
//!   never a secret value.
//! - Secret values travel as [`SecretString`] (`secrecy`), which is zeroized on drop and never
//!   printed by `Debug`.
//! - Errors never carry a secret value (see [`SecretError`], [`VaultError`]).
//! - There is no plaintext store: without a Secret Service, secrets go to the vault or are asked
//!   every time ([`backend`]).
//!
//! Store access is **blocking**. Callers run it on a blocking thread (for example
//! `tokio::task::spawn_blocking`), never on the GPUI foreground thread (I6) and never directly on
//! an async worker thread.
#![forbid(unsafe_code)]

pub mod backend;
mod error;
#[cfg(any(test, feature = "memory-store"))]
mod memory;
mod project;
mod reference;
mod registry;
#[cfg(feature = "secret-service")]
mod secret_service;
mod session_cache;
mod store;
pub mod vault;

pub use backend::{SecretServiceAvailability, detect_secret_service};
pub use error::SecretError;
#[cfg(any(test, feature = "memory-store"))]
pub use memory::MemoryStore;
pub use project::InvalidProjectId;
pub use reference::{InvalidSecretId, SecretId, SecretKind, SecretRef, StoreKind};
pub use registry::SecretStores;
pub use secrecy::{ExposeSecret, SecretString};
#[cfg(feature = "secret-service")]
pub use secret_service::SecretServiceStore;
pub use session_cache::{CacheLifetime, SessionSecretCache};
pub use store::{SecretMeta, SecretStore};
pub use vault::{KdfParams, UnlockBackoff, Vault, VaultError, VaultStatus, VaultStore};

/// Service name used for every item NineSQL writes to a secret store (ADR-0005).
pub const SERVICE: &str = "ninesql";
