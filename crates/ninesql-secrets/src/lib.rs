//! Secrets: secret references, the [`SecretStore`] trait, the encrypted [`Vault`], the
//! Secret Service backend (feature `secret-service`), backend selection, the "ask every time"
//! [`SessionSecretCache`], locking on sleep / session lock ([`lock_events`], with the logind
//! watcher behind the default-on feature `desktop-lock`) and an in-memory test
//! store (ADR-0005, invariant I1). ROADMAP: P0-D,
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
mod clock;
mod error;
pub mod lock_events;
#[cfg(feature = "desktop-lock")]
mod lock_signals;
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
pub use lock_events::{LockHook, LockHooks, LockTrigger};
#[cfg(feature = "desktop-lock")]
pub use lock_signals::{
    DesktopLockWatcher, LockWatchError, RECONNECT_FIRST, RECONNECT_MAX, WATCHER_SETUP_TIMEOUT,
    WatcherState,
};
#[cfg(any(test, feature = "memory-store"))]
pub use memory::MemoryStore;
pub use project::InvalidProjectId;
pub use reference::{InvalidSecretId, SecretId, SecretKind, SecretRef, StoreKind};
pub use registry::SecretStores;
pub use secrecy::{ExposeSecret, SecretString};
#[cfg(feature = "secret-service")]
pub use secret_service::SecretServiceStore;
pub use session_cache::{CacheLifetime, MAX_TIMED_LIFETIME, SessionSecretCache};
pub use store::{SecretMeta, SecretStore};
pub use vault::{
    KdfParams, MIN_MASTER_PASSWORD_CHARS, UnlockBackoff, Vault, VaultError, VaultStatus, VaultStore,
};

/// Service name used for every item NineSQL writes to a secret store (ADR-0005).
pub const SERVICE: &str = "ninesql";
