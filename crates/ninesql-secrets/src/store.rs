//! The secret store trait.

use secrecy::SecretString;

use crate::error::SecretError;
use crate::reference::{SecretId, SecretKind, StoreKind};

/// Non-secret metadata attached to a stored secret (Secret Service attributes, ADR-0005 §1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretMeta {
    /// Identifier of the connection the secret belongs to.
    pub connection_id: String,
    /// What the secret is used for.
    pub kind: SecretKind,
    /// Human-readable label shown by keyring managers (for example the connection name).
    /// Not secret; never put a value in it.
    pub label: String,
}

/// A place where secrets live: Secret Service, the encrypted vault, or memory in tests.
///
/// All methods are blocking (see the crate documentation).
pub trait SecretStore: Send + Sync {
    /// Which store this is; references with this [`StoreKind`] are routed here.
    fn kind(&self) -> StoreKind;

    /// Reads a secret.
    ///
    /// # Errors
    /// [`SecretError::NotFound`] when nothing is stored under `id`, or a backend error.
    fn get(&self, id: &SecretId) -> Result<SecretString, SecretError>;

    /// Stores or replaces a secret.
    ///
    /// # Errors
    /// A backend error; the secret is not stored.
    fn set(
        &self,
        id: &SecretId,
        meta: &SecretMeta,
        secret: &SecretString,
    ) -> Result<(), SecretError>;

    /// Deletes a secret. Returns `false` when nothing was stored under `id`.
    ///
    /// # Errors
    /// A backend error.
    fn delete(&self, id: &SecretId) -> Result<bool, SecretError>;
}
