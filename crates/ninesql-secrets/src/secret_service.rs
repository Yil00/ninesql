//! **Spike** (feature `secret-service`): freedesktop Secret Service backend (ADR-0005 §1) on the
//! pure-Rust `secret-service` 6 crate (zbus 6, Diffie-Hellman session with Rust crypto). No
//! `libdbus`, `libsecret` or OpenSSL.
//!
//! Status: compiles, error mapping unit-tested, verified to report [`SecretError::Unavailable`]
//! on a session bus without provider. The round trip against GNOME Keyring and KWallet on
//! Debian 12 / Ubuntu 24.04 is still to be validated on real desktops (ROADMAP P0-D).
//!
//! ADR-0005 names the `keyring` crate; it is not used because keyring 3 depends on zbus 4
//! (unmaintained advisories RUSTSEC-2026-0370/0371/0372) and keyring 4's store crates use
//! zbus 5 / secret-service 5, flagged the same way. See the P0-D task report.
//!
//! Items go to the default collection with attributes `xdg:schema = "org.ninesql.Secret"`,
//! `service = "ninesql"`, `project`, `secret_id`, `connection_id` and `kind`. Lookups use
//! `xdg:schema` + `service` + `project` + `secret_id`, so a project copied elsewhere (same
//! connection ids) never reads or overwrites another project's secrets. Writing deletes any
//! existing item for the id first, so a changed `connection_id` / `kind` cannot leave a stale
//! duplicate behind.
//!
//! Calls block the current thread (zbus runs its own executor); run them on a blocking thread,
//! never on the GPUI foreground.

use std::collections::HashMap;

use secrecy::{ExposeSecret, SecretString};
use secret_service::EncryptionType;
use secret_service::blocking::{Item, SecretService};
use zeroize::Zeroizing;

use crate::SERVICE;
use crate::error::SecretError;
use crate::reference::{SecretId, StoreKind};
use crate::store::{SecretMeta, SecretStore};

const ATTR_SCHEMA: &str = "xdg:schema";
/// Value of the `xdg:schema` attribute of NineSQL items.
pub const SCHEMA: &str = "org.ninesql.Secret";
const ATTR_SERVICE: &str = "service";
const ATTR_PROJECT: &str = "project";
const ATTR_SECRET_ID: &str = "secret_id";
const ATTR_CONNECTION_ID: &str = "connection_id";
const ATTR_KIND: &str = "kind";

/// [`SecretStore`] backed by the freedesktop Secret Service, scoped to one workspace project.
#[derive(Debug, Clone)]
pub struct SecretServiceStore {
    project: String,
}

impl SecretServiceStore {
    /// A handle on the session's Secret Service for the items of `project` (the workspace
    /// project name, not secret). Connects on each call.
    #[must_use]
    pub fn new(project: impl Into<String>) -> Self {
        Self {
            project: project.into(),
        }
    }

    fn lookup<'a>(&'a self, id: &'a SecretId) -> HashMap<&'a str, &'a str> {
        HashMap::from([
            (ATTR_SCHEMA, SCHEMA),
            (ATTR_SERVICE, SERVICE),
            (ATTR_PROJECT, self.project.as_str()),
            (ATTR_SECRET_ID, id.as_str()),
        ])
    }

    /// Every item stored under `id` in this project.
    fn search<'a>(
        &self,
        ss: &'a SecretService<'a>,
        id: &SecretId,
    ) -> Result<Vec<Item<'a>>, SecretError> {
        let found = ss.search_items(self.lookup(id)).map_err(map_error)?;
        let mut items = found.unlocked;
        items.extend(found.locked);
        Ok(items)
    }

    fn connect<'a>() -> Result<SecretService<'a>, SecretError> {
        SecretService::connect(EncryptionType::Dh).map_err(map_error)
    }

    /// The single item stored under `id`, if any.
    fn find<'a>(
        &self,
        ss: &'a SecretService<'a>,
        id: &SecretId,
    ) -> Result<Option<Item<'a>>, SecretError> {
        let mut items = self.search(ss, id)?;
        match items.len() {
            0 => Ok(None),
            1 => Ok(items.pop()),
            _ => Err(SecretError::Ambiguous(id.clone())),
        }
    }
}

impl SecretStore for SecretServiceStore {
    fn kind(&self) -> StoreKind {
        StoreKind::Keyring
    }

    fn get(&self, id: &SecretId) -> Result<SecretString, SecretError> {
        let ss = Self::connect()?;
        let item = self
            .find(&ss, id)?
            .ok_or_else(|| SecretError::NotFound(id.clone()))?;
        item.ensure_unlocked().map_err(map_error)?;
        let bytes = Zeroizing::new(item.get_secret().map_err(map_error)?);
        let text = std::str::from_utf8(&bytes).map_err(|_| SecretError::BadEncoding(id.clone()))?;
        // `to_owned` allocates exactly `len` bytes, so the boxing inside `SecretString::from`
        // does not reallocate and leave an unzeroized copy behind.
        Ok(SecretString::from(text.to_owned()))
    }

    fn set(
        &self,
        id: &SecretId,
        meta: &SecretMeta,
        secret: &SecretString,
    ) -> Result<(), SecretError> {
        let ss = Self::connect()?;
        let collection = ss.get_default_collection().map_err(map_error)?;
        collection.ensure_unlocked().map_err(map_error)?;
        // Delete-before-create: never leave an older item with other attributes behind.
        for old in self.search(&ss, id)? {
            old.delete().map_err(map_error)?;
        }
        let mut attributes = self.lookup(id);
        attributes.insert(ATTR_CONNECTION_ID, meta.connection_id.as_str());
        attributes.insert(ATTR_KIND, meta.kind.as_str());
        collection
            .create_item(
                &meta.label,
                attributes,
                secret.expose_secret().as_bytes(),
                true, // replace the item with the same attributes
                "text/plain",
            )
            .map_err(map_error)?;
        Ok(())
    }

    fn delete(&self, id: &SecretId) -> Result<bool, SecretError> {
        let ss = Self::connect()?;
        match self.find(&ss, id)? {
            Some(item) => {
                item.delete().map_err(map_error)?;
                Ok(true)
            }
            None => Ok(false),
        }
    }
}

/// Maps a Secret Service error. None of its variants carries a secret value.
fn map_error(error: secret_service::Error) -> SecretError {
    use secret_service::Error as E;
    let store = StoreKind::Keyring;
    match error {
        E::Locked | E::Prompt | E::PromptDisconnected => SecretError::Locked(store),
        E::Unavailable => SecretError::Unavailable {
            store,
            detail: "no Secret Service provider answers on the session bus".to_owned(),
        },
        E::Zbus(e) => SecretError::Unavailable {
            store,
            detail: e.to_string(),
        },
        E::ZbusFdo(e) => SecretError::Unavailable {
            store,
            detail: e.to_string(),
        },
        E::NoResult => SecretError::Backend {
            store,
            detail: "the Secret Service has no default collection".to_owned(),
        },
        E::Crypto(e) => SecretError::Backend {
            store,
            detail: format!("session encryption failed: {e}"),
        },
        _ => SecretError::Backend {
            store,
            detail: "unrecognized Secret Service error".to_owned(),
        },
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).
mod tests {
    use super::*;
    use crate::reference::SecretKind;

    fn id() -> SecretId {
        SecretId::new("spike:password").unwrap()
    }

    #[test]
    fn maps_common_errors() {
        use secret_service::Error as E;
        assert!(matches!(
            map_error(E::Locked),
            SecretError::Locked(StoreKind::Keyring)
        ));
        assert!(matches!(map_error(E::Prompt), SecretError::Locked(_)));
        assert!(matches!(
            map_error(E::Unavailable),
            SecretError::Unavailable { .. }
        ));
        assert!(matches!(
            map_error(E::NoResult),
            SecretError::Backend { .. }
        ));
        assert!(matches!(
            map_error(E::Crypto("x")),
            SecretError::Backend { .. }
        ));
    }

    #[test]
    fn lookup_attributes() {
        let id = id();
        let store = SecretServiceStore::new("General");
        let attrs = store.lookup(&id);
        assert_eq!(attrs.get("xdg:schema"), Some(&"org.ninesql.Secret"));
        assert_eq!(attrs.get("service"), Some(&"ninesql"));
        assert_eq!(attrs.get("project"), Some(&"General"));
        assert_eq!(attrs.get("secret_id"), Some(&"spike:password"));
        assert_eq!(attrs.len(), 4);
    }

    /// Round trip against a real Secret Service. Ignored by default: run it on a desktop session
    /// with `cargo test -p ninesql-secrets --features secret-service -- --ignored`.
    #[test]
    #[ignore = "needs a session bus with a Secret Service (GNOME Keyring, KWallet, KeePassXC)"]
    fn round_trip_on_a_real_secret_service() {
        let store = SecretServiceStore::new("ninesql-spike-test");
        let id = SecretId::new("ninesql-spike-test:password").unwrap();
        let meta = SecretMeta {
            connection_id: "ninesql-spike-test".into(),
            kind: SecretKind::Password,
            label: "NineSQL spike test".into(),
        };
        store
            .set(&id, &meta, &SecretString::from("spike-secret".to_owned()))
            .unwrap();
        assert_eq!(store.get(&id).unwrap().expose_secret(), "spike-secret");
        // Replacing keeps a single item.
        store
            .set(&id, &meta, &SecretString::from("spike-secret-2".to_owned()))
            .unwrap();
        assert_eq!(store.get(&id).unwrap().expose_secret(), "spike-secret-2");
        assert!(store.delete(&id).unwrap());
        assert!(!store.delete(&id).unwrap());
        assert!(matches!(store.get(&id), Err(SecretError::NotFound(_))));
    }

    /// Without any Secret Service provider the store reports itself unavailable instead of
    /// failing silently or panicking. Opt-in: run under `dbus-run-session` with
    /// `NINESQL_TEST_EXPECT_NO_SECRET_SERVICE=1`.
    #[test]
    fn reports_unavailable_without_provider() {
        if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none()
            || std::env::var_os("NINESQL_TEST_EXPECT_NO_SECRET_SERVICE").is_none()
        {
            return;
        }
        let err = SecretServiceStore::new("General").get(&id()).unwrap_err();
        assert!(matches!(err, SecretError::Unavailable { .. }), "{err:?}");
    }
}
