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
