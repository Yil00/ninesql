//! Errors of the secret stores. No variant carries a secret value.

use crate::reference::{SecretId, StoreKind};

/// Error returned by a [`crate::SecretStore`].
///
/// Messages are user-presentable and **never** include a secret value. Backend detail strings
/// come from the store transport (D-Bus error names and messages), never from the secret payload.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SecretError {
    /// No secret is stored under this identifier.
    #[error("no secret is stored for identifier `{0}`")]
    NotFound(SecretId),
    /// The reference points to a store that is not configured in this session.
    #[error("the {0} secret store is not configured")]
    StoreNotConfigured(StoreKind),
    /// The store exists but refused access (locked collection, dismissed prompt).
    #[error("the {0} secret store is locked or refused access")]
    Locked(StoreKind),
    /// The store could not be reached (no Secret Service on the session bus, no session bus).
    #[error("the {store} secret store is not available: {detail}")]
    Unavailable {
        /// Store that failed.
        store: StoreKind,
        /// Transport-level detail, without any secret value.
        detail: String,
    },
    /// The stored value is not valid UTF-8. The bytes are deliberately not kept.
    #[error("the secret stored for identifier `{0}` is not valid UTF-8")]
    BadEncoding(SecretId),
    /// More than one item matches the identifier; NineSQL refuses to guess.
    #[error("more than one secret matches identifier `{0}`")]
    Ambiguous(SecretId),
    /// Any other backend failure, without any secret value.
    #[error("the {store} secret store failed: {detail}")]
    Backend {
        /// Store that failed.
        store: StoreKind,
        /// Backend detail, without any secret value.
        detail: String,
    },
}

impl SecretError {
    /// A short, stable name of the error kind, safe to log at any level: no identifier, no
    /// transport detail (ADR-0029 §3.2). The message itself is for the UI.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::NotFound(_) => "secret-not-found",
            Self::StoreNotConfigured(_) => "secret-store-not-configured",
            Self::Locked(_) => "secret-store-locked",
            Self::Unavailable { .. } => "secret-store-unavailable",
            Self::BadEncoding(_) => "secret-bad-encoding",
            Self::Ambiguous(_) => "secret-ambiguous",
            Self::Backend { .. } => "secret-store-failed",
        }
    }
}
