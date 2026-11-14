//! Routing of [`SecretRef`]s to the configured stores.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use secrecy::SecretString;

use crate::error::SecretError;
use crate::reference::{SecretRef, StoreKind};
use crate::store::{SecretMeta, SecretStore};

/// The secret stores available in this session, one per [`StoreKind`].
#[derive(Default, Clone)]
pub struct SecretStores {
    stores: BTreeMap<StoreKind, Arc<dyn SecretStore>>,
}

impl fmt::Debug for SecretStores {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretStores")
            .field("kinds", &self.stores.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl SecretStores {
    /// No store configured.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a store, replacing any previous store of the same kind.
    pub fn register(&mut self, store: Arc<dyn SecretStore>) {
        self.stores.insert(store.kind(), store);
    }

    /// The store of a given kind.
    ///
    /// # Errors
    /// [`SecretError::StoreNotConfigured`] when no store of that kind is registered.
    pub fn store(&self, kind: StoreKind) -> Result<&Arc<dyn SecretStore>, SecretError> {
        self.stores
            .get(&kind)
            .ok_or(SecretError::StoreNotConfigured(kind))
    }

    /// Whether a store of that kind is registered.
    #[must_use]
    pub fn has(&self, kind: StoreKind) -> bool {
        self.stores.contains_key(&kind)
    }

    /// Where a newly saved secret goes: the keyring when a keyring store is registered, else the
    /// vault when one is registered, else `None` (the password cannot be saved; the core offers
    /// "ask every time" only). There is no plaintext option (I1); see [`crate::backend`].
    #[must_use]
    pub fn store_for_new_secrets(&self) -> Option<StoreKind> {
        [StoreKind::Keyring, StoreKind::Vault]
            .into_iter()
            .find(|kind| self.has(*kind))
    }

    /// Reads the secret behind a reference.
    ///
    /// # Errors
    /// See [`SecretStore::get`] and [`SecretStores::store`].
    pub fn resolve(&self, reference: &SecretRef) -> Result<SecretString, SecretError> {
        self.store(reference.store)?.get(&reference.id)
    }

    /// Stores a secret behind a reference.
    ///
    /// # Errors
    /// See [`SecretStore::set`] and [`SecretStores::store`].
    pub fn save(
        &self,
        reference: &SecretRef,
        meta: &SecretMeta,
        secret: &SecretString,
    ) -> Result<(), SecretError> {
        tracing::debug!(reference = %reference, kind = %meta.kind, "saving secret");
        self.store(reference.store)?
            .set(&reference.id, meta, secret)
    }

    /// Deletes the secret behind a reference. Returns `false` when nothing was stored.
    ///
    /// # Errors
    /// See [`SecretStore::delete`] and [`SecretStores::store`].
    pub fn delete(&self, reference: &SecretRef) -> Result<bool, SecretError> {
        tracing::debug!(reference = %reference, "deleting secret");
        self.store(reference.store)?.delete(&reference.id)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).
mod tests {
    use secrecy::ExposeSecret;

    use super::*;
    use crate::memory::MemoryStore;
    use crate::reference::{SecretId, SecretKind};

    fn meta() -> SecretMeta {
        SecretMeta {
            connection_id: "c1".into(),
            kind: SecretKind::Password,
            label: "test".into(),
        }
    }

    #[test]
    fn routes_by_store_kind() {
        let mut stores = SecretStores::new();
        stores.register(Arc::new(MemoryStore::new(StoreKind::Keyring)));
        let r = SecretRef::new(StoreKind::Keyring, SecretId::new("c1:password").unwrap());
        stores
            .save(&r, &meta(), &SecretString::from("hunter2".to_owned()))
            .unwrap();
        assert_eq!(stores.resolve(&r).unwrap().expose_secret(), "hunter2");

        let v = SecretRef::new(StoreKind::Vault, SecretId::new("c1:password").unwrap());
        assert!(matches!(
            stores.resolve(&v),
            Err(SecretError::StoreNotConfigured(StoreKind::Vault))
        ));
        assert!(stores.delete(&r).unwrap());
        assert!(!stores.delete(&r).unwrap());
    }

    #[test]
    fn new_secrets_prefer_keyring_then_vault_never_plaintext() {
        let mut stores = SecretStores::new();
        assert_eq!(stores.store_for_new_secrets(), None);
        stores.register(Arc::new(MemoryStore::new(StoreKind::Vault)));
        assert_eq!(stores.store_for_new_secrets(), Some(StoreKind::Vault));
        stores.register(Arc::new(MemoryStore::new(StoreKind::Keyring)));
        assert_eq!(stores.store_for_new_secrets(), Some(StoreKind::Keyring));
    }

    #[test]
    fn debug_does_not_show_contents() {
        let mut stores = SecretStores::new();
        let mem = Arc::new(MemoryStore::new(StoreKind::Keyring));
        stores.register(mem);
        let r = SecretRef::new(StoreKind::Keyring, SecretId::new("x").unwrap());
        stores
            .save(&r, &meta(), &SecretString::from("hunter2".to_owned()))
            .unwrap();
        assert!(!format!("{stores:?}").contains("hunter2"));
    }
}
