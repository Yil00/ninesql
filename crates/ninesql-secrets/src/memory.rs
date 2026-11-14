//! In-memory secret store for tests (feature `memory-store`). Never use it in the application:
//! secrets vanish at exit.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

use secrecy::{ExposeSecret, SecretString};

use crate::error::SecretError;
use crate::reference::{SecretId, StoreKind};
use crate::store::{SecretMeta, SecretStore};

/// In-memory [`SecretStore`] that pretends to be a store of the given kind.
pub struct MemoryStore {
    kind: StoreKind,
    items: Mutex<HashMap<SecretId, (SecretMeta, SecretString)>>,
}

impl std::fmt::Debug for MemoryStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryStore")
            .field("kind", &self.kind)
            .field("len", &self.len())
            .finish_non_exhaustive()
    }
}

impl MemoryStore {
    /// An empty store answering for `kind`.
    #[must_use]
    pub fn new(kind: StoreKind) -> Self {
        Self {
            kind,
            items: Mutex::new(HashMap::new()),
        }
    }

    fn items(&self) -> MutexGuard<'_, HashMap<SecretId, (SecretMeta, SecretString)>> {
        // A panic while holding the lock cannot leave the map half-updated: recover it.
        self.items
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Number of stored secrets.
    #[must_use]
    pub fn len(&self) -> usize {
        self.items().len()
    }

    /// Whether the store is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Metadata stored with a secret, if any.
    #[must_use]
    pub fn meta(&self, id: &SecretId) -> Option<SecretMeta> {
        self.items().get(id).map(|(meta, _)| meta.clone())
    }
}

impl SecretStore for MemoryStore {
    fn kind(&self) -> StoreKind {
        self.kind
    }

    fn get(&self, id: &SecretId) -> Result<SecretString, SecretError> {
        self.items()
            .get(id)
            .map(|(_, s)| SecretString::from(s.expose_secret().to_owned()))
            .ok_or_else(|| SecretError::NotFound(id.clone()))
    }

    fn set(
        &self,
        id: &SecretId,
        meta: &SecretMeta,
        secret: &SecretString,
    ) -> Result<(), SecretError> {
        let copy = SecretString::from(secret.expose_secret().to_owned());
        self.items().insert(id.clone(), (meta.clone(), copy));
        Ok(())
    }

    fn delete(&self, id: &SecretId) -> Result<bool, SecretError> {
        Ok(self.items().remove(id).is_some())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).
mod tests {
    use super::*;
    use crate::reference::SecretKind;

    #[test]
    fn round_trip_and_not_found() {
        let store = MemoryStore::new(StoreKind::Vault);
        let id = SecretId::new("a").unwrap();
        assert!(matches!(store.get(&id), Err(SecretError::NotFound(_))));
        let meta = SecretMeta {
            connection_id: "c".into(),
            kind: SecretKind::Token,
            label: "l".into(),
        };
        store
            .set(&id, &meta, &SecretString::from("t0k".to_owned()))
            .unwrap();
        assert_eq!(store.get(&id).unwrap().expose_secret(), "t0k");
        assert_eq!(store.meta(&id), Some(meta));
        assert!(!format!("{store:?}").contains("t0k"));
        assert!(store.delete(&id).unwrap());
        assert!(store.is_empty());
    }
}
