//! Freedesktop Secret Service backend (feature `secret-service`, ADR-0005 §1, ADR-0012) on the
//! pure-Rust `secret-service` 6 crate (zbus 6, Diffie-Hellman session with Rust crypto). No
//! `libdbus`, `libsecret` or OpenSSL.
//!
//! Status: compiles, error mapping unit-tested, verified to report [`SecretError::Unavailable`]
//! (and [`crate::detect_secret_service`] to report `Unavailable`) on a session bus without
//! provider and without any session bus. The round trip against GNOME Keyring and KWallet on
//! Debian 12 / Ubuntu 24.04 is still to be validated on real desktops (ROADMAP P0-D). The
//! feature stays **off by default** until that round trip and the review below are closed.
//!
//! # Review of `secret-service` 6.0.0-rc.1 key handling (P0-D review item L5)
//! Reviewed source: `src/session.rs`, `src/util.rs`, `src/item.rs`, `src/collection.rs` of the
//! crates.io package `secret-service-6.0.0-rc.1` (features `rt-default`, `crypto-rust`).
//!
//! Session protocol: `dh-ietf1024-sha256-aes128-cbc-pkcs7` as defined by the Secret Service
//! specification (1024-bit MODP group 2, HKDF-SHA256 without salt, AES-128-CBC, no MAC). The
//! weak group and the missing MAC are properties of the specification, not of the crate; the
//! encryption only keeps secrets out of D-Bus monitors and message logs on the local bus.
//!
//! Key material that is **not zeroized** by the crate:
//! 1. `Keypair::generate`: the 128 random bytes of the DH private exponent (`[u8; 128]` on the
//!    stack) and the private key as a `num-bigint` `BigUint` (heap; `num-bigint` 0.4 has no
//!    zeroize support). `powm` clones the exponent into temporaries that are freed unwiped.
//!    `powm` is a square-and-multiply on `BigUint`, not constant time.
//! 2. `Keypair::derive_shared`: the shared secret (`BigUint`, then its big-endian bytes in a
//!    `Vec<u8>`), the HKDF state (`hkdf` 0.13 / `hmac` 0.13 have no zeroize feature) and the
//!    16-byte output copy on the stack.
//! 3. `Session::aes_key` (`hybrid_array::Array<u8, U16>`): kept for the life of the
//!    `SecretService` value and freed without wiping (no `Drop` impl).
//! 4. Secret payloads: `get_secret` returns the decrypted secret in a fresh `Vec<u8>` (NineSQL
//!    wraps it in `Zeroizing` at once, which also wipes the PKCS#7 padding left in the spare
//!    capacity); `format_secret` encrypts in a buffer that then holds only ciphertext. With a
//!    *plain* session the crate would copy the secret unwiped (`secret.to_vec()`); NineSQL always
//!    asks for [`EncryptionType::Dh`].
//! 5. `getrandom` failures `expect` (panic) inside the crate instead of returning an error.
//!    [`crate::detect_secret_service`] runs the probe on its own thread and reports a panic
//!    there as `Unavailable`; store calls still propagate such a panic to their caller.
//! 6. The AES-128 key schedule held by the `cbc` `Encryptor` / `Decryptor` that encrypt and
//!    decrypt each secret is only wiped when the `aes` crate is built with its `zeroize`
//!    feature. NineSQL enables it through feature unification (an otherwise unused `aes`
//!    dependency of this crate, behind the `secret-service` feature), so those round keys are
//!    zeroized on drop.
//!
//! Side effects of the probe and of the first call (to check in the ADR-0012 desktop round
//! trip): connecting to `org.freedesktop.secrets` can make the session bus **D-Bus-activate** a
//! provider that is installed but not running (`gnome-keyring-daemon`, `ksecretd` /
//! `kwalletd`). On KDE this may start the KWallet first-run wizard or a "create wallet" dialog,
//! even though NineSQL reads only the `default` alias and asks for no unlock.
//!
//! Impact: the leftovers are an ephemeral DH key and an AES session key that only decrypt the
//! D-Bus traffic of one short-lived session (this backend connects, and so negotiates new keys,
//! on every call). Reading them needs access to NineSQL's memory, a core dump or swap; an
//! attacker with that access can already read the secrets themselves (docs/05-security.md,
//! "What NineSQL does not protect against"). No stored secret or master key is left behind by
//! NineSQL's own code: values are borrowed from [`SecretString`] when writing and wrapped in
//! [`Zeroizing`] / [`SecretString`] when reading.
//!
//! Recommendation: acceptable as a documented residual risk once a security reviewer agrees;
//! report items 1–3 upstream (zeroize the key pair, the shared secret and `Session::aes_key` on
//! drop). Keep the feature off by default until (a) that review is closed, (b) the desktop round
//! trip of ADR-0012 passes, and (c) the move from the release candidate to 6.0.0 is decided.
//!
//! Items go to the default collection with attributes `xdg:schema = "org.ninesql.Secret"`,
//! `service = "ninesql"`, `project`, `secret_id`, `connection_id` and `kind`. Lookups use
//! `xdg:schema` + `service` + `project` + `secret_id`.
//!
//! The **`project` attribute holds the project's stable UUID** (the `id` of `project.json`,
//! `ninesql_core::ProjectId`), never the project name: a renamed project keeps its secrets, and
//! two projects with different ids (same connection ids) never read or overwrite each other's
//! secrets (review item N6). [`SecretServiceStore::new`] refuses anything that is not a UUID.
//! Items written by earlier development builds under the project *name* are not migrated: the
//! backend never shipped (it is a spike behind a feature flag).
//!
//! Writing stores the new item first (replacing one with the same attributes), then deletes the
//! other items under the same id, such as a stale duplicate with an older `connection_id` /
//! `kind`. A failed write therefore never loses the previous credential, and a stale duplicate
//! that cannot be deleted (locked, prompt dismissed) is logged, not fatal. If the new item is not
//! among the items found under the id afterwards, nothing is deleted (the search result cannot be
//! trusted to tell stale items from the new one).
//!
//! Reading with duplicates under one id (a stale item that could not be deleted) picks the single
//! item that is in the default collection **and** whose `connection_id` / `kind` attributes match
//! the id (`<connection uuid>:<slot>`, kind allowed for the slot); if there is not exactly one,
//! the read fails with [`SecretError::Ambiguous`] rather than guess.
//!
//! Deleting removes every item stored under the id in this project, duplicates included; it
//! fails only if the default-collection item (the one reads use) could not be deleted, or if
//! nothing could be deleted. If the default collection cannot be looked up, every item is
//! treated as non-default and deletion still proceeds.
//!
//! Calls block the current thread (zbus runs its own executor); run them on a blocking thread,
//! never on the GPUI foreground.
//!
//! # Timeouts
//! zbus 6 sets no method-call timeout by default, so a provider that never replies would block a
//! call forever. Every connection this backend opens sets one ([`METHOD_TIMEOUT`], 25 s, the
//! usual `dbus-daemon` reply timeout); a call that runs out is [`SecretError::Unavailable`]. The
//! startup probe is bounded separately ([`PROBE_TIMEOUT`], 5 s, see [`crate::detect_secret_service`]).
//! A call that has to wait for an unlock **prompt** waits for the prompt's `Completed` signal,
//! which is not a method call: the user may take any time. The core should therefore run store
//! calls with its own timeout or cancel path (for example `tokio::time::timeout` around
//! `spawn_blocking`) and map a `JoinError` (a panic inside the client) to
//! [`SecretError::Unavailable`].

use std::collections::HashMap;
use std::sync::mpsc;
use std::time::Duration;

use secrecy::{ExposeSecret, SecretString};
use secret_service::EncryptionType;
use secret_service::blocking::{Item, SecretService};
use zeroize::Zeroizing;

use crate::SERVICE;
use crate::error::SecretError;
use crate::project::{InvalidProjectId, normalize_project_id};
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

/// Timeout of each D-Bus method call to the Secret Service.
pub const METHOD_TIMEOUT: Duration = Duration::from_secs(25);
/// Longest wait for [`crate::detect_secret_service`].
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// [`SecretStore`] backed by the freedesktop Secret Service, scoped to one workspace project.
#[derive(Debug, Clone)]
pub struct SecretServiceStore {
    /// Project UUID, lower-case hyphenated: value of the `project` attribute.
    project: String,
}

impl SecretServiceStore {
    /// A handle on the session's Secret Service for the items of one project, identified by its
    /// stable UUID from `project.json` (`workspace.project_id().to_string()`), **not** its name.
    /// The UUID becomes the `project` attribute of every item, lower-cased. Connects on each
    /// call.
    ///
    /// # Errors
    /// [`InvalidProjectId`] unless `project_id` is a hyphenated UUID (`8-4-4-4-12` hex digits),
    /// so a project name cannot be passed by mistake.
    pub fn new(project_id: &str) -> Result<Self, InvalidProjectId> {
        Ok(Self {
            project: normalize_project_id(project_id)?,
        })
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

    /// A session-bus connection with [`METHOD_TIMEOUT`] on every method call, then an encrypted
    /// (Diffie-Hellman) Secret Service session on it.
    fn connect<'a>() -> Result<SecretService<'a>, SecretError> {
        let conn = zbus::block_on(
            zbus::connection::Builder::session()
                .method_timeout(METHOD_TIMEOUT)
                .build(),
        )
        .map_err(|e| map_error(e.into()))?;
        SecretService::connect_with_existing(EncryptionType::Dh, conn).map_err(map_error)
    }

    /// The item to read under `id`: the only one, or among duplicates the single one that
    /// [`pick_duplicate`] accepts.
    fn find_for_read<'a>(
        &self,
        ss: &'a SecretService<'a>,
        id: &SecretId,
    ) -> Result<Option<Item<'a>>, SecretError> {
        let mut items = self.search(ss, id)?;
        if items.len() <= 1 {
            return Ok(items.pop());
        }
        let in_default: Vec<String> = ss
            .get_default_collection()
            .and_then(|c| {
                c.search_items(self.lookup(id))
                    .map(|found| found.iter().map(|i| i.item_path.to_string()).collect())
            })
            .map_err(map_error)?;
        let candidates: Vec<Candidate> = items
            .iter()
            .map(|item| Candidate {
                in_default_collection: in_default.contains(&item.item_path.to_string()),
                // An item whose attributes cannot be read is never picked.
                attributes: item.get_attributes().unwrap_or_default(),
            })
            .collect();
        match pick_duplicate(id, &candidates) {
            Some(index) => {
                tracing::warn!(id = %id, duplicates = items.len(), "several secret items share an id; reading the default-collection item that matches it");
                Ok(Some(items.swap_remove(index)))
            }
            None => Err(SecretError::Ambiguous(id.clone())),
        }
    }
}

/// Probe for [`crate::detect_secret_service`]: connects with an encrypted session and reads the
/// `default` collection alias. Transfers no secret and triggers no unlock prompt (but may
/// D-Bus-activate a provider, see the module documentation).
///
/// Runs on a thread of its own and waits at most [`PROBE_TIMEOUT`]: a panic inside
/// `secret-service` (item 5 of the review) or a provider that does not answer in time becomes
/// `Unavailable` instead of unwinding into, or blocking, the caller.
pub(crate) fn probe() -> crate::backend::SecretServiceAvailability {
    probe_with(probe_on_this_thread, PROBE_TIMEOUT)
}

/// Runs `probe` on a new thread and waits for it at most `timeout`. On timeout the thread is
/// left detached (one per call; the core probes once per startup) and finishes or stays blocked
/// on its own, holding no secret.
fn probe_with<F>(probe: F, timeout: Duration) -> crate::backend::SecretServiceAvailability
where
    F: FnOnce() -> crate::backend::SecretServiceAvailability + Send + 'static,
{
    use crate::backend::SecretServiceAvailability as A;
    let unavailable = |detail: &str| A::Unavailable {
        detail: detail.to_owned(),
    };
    let (tx, rx) = mpsc::sync_channel(1);
    let spawned = std::thread::Builder::new()
        .name("ninesql-secret-service-probe".into())
        .spawn(move || {
            // The receiver is gone after a timeout: nothing left to report to.
            let _ = tx.send(probe());
        });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "could not start the Secret Service probe");
        return unavailable("could not start the Secret Service probe");
    }
    match rx.recv_timeout(timeout) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            tracing::warn!(
                timeout_s = timeout.as_secs_f32(),
                "the Secret Service did not answer in time; using the encrypted vault"
            );
            unavailable("timed out")
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            // The sender was dropped without a result: the probe panicked.
            tracing::warn!("the Secret Service probe panicked; using the encrypted vault");
            unavailable("the Secret Service client failed unexpectedly")
        }
    }
}

fn probe_on_this_thread() -> crate::backend::SecretServiceAvailability {
    use crate::backend::SecretServiceAvailability as A;
    let result = SecretServiceStore::connect()
        .and_then(|ss| ss.get_default_collection().map(|_| ()).map_err(map_error));
    match result {
        Ok(()) => A::Available,
        Err(e) => {
            tracing::info!(error = %e, "no usable Secret Service; the encrypted vault is the secret store");
            A::Unavailable {
                detail: e.to_string(),
            }
        }
    }
}

/// What [`pick_duplicate`] knows of one item found under a secret id.
#[derive(Debug, Default)]
struct Candidate {
    /// The item is in the default collection (where NineSQL writes).
    in_default_collection: bool,
    /// Its attributes.
    attributes: HashMap<String, String>,
}

/// Kinds a slot of a connection secret id (`<connection uuid>:<slot>`) can hold.
fn kinds_for_slot(slot: &str) -> &'static [&'static str] {
    use crate::reference::SecretKind as K;
    const AUTH: &[&str] = &[K::Password.as_str(), K::Token.as_str()];
    const SSH: &[&str] = &[K::SshPassword.as_str(), K::SshKeyPassphrase.as_str()];
    const TLS: &[&str] = &[K::ClientKeyPassphrase.as_str()];
    const PROXY: &[&str] = &[K::ProxyPassword.as_str()];
    match slot {
        "auth" => AUTH,
        "ssh" => SSH,
        "tls-client-key" => TLS,
        "proxy" => PROXY,
        s if s
            .strip_prefix("ssh-jump-")
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())) =>
        {
            SSH
        }
        _ => &[],
    }
}

/// Among several items stored under `id`, the index of the single one that is in the default
/// collection and whose `connection_id` and `kind` attributes match the id; `None` when there is
/// not exactly one (the caller reports [`SecretError::Ambiguous`]).
fn pick_duplicate(id: &SecretId, candidates: &[Candidate]) -> Option<usize> {
    let (connection_id, slot) = id.as_str().split_once(':')?;
    let kinds = kinds_for_slot(slot);
    let mut matching = candidates.iter().enumerate().filter(|(_, c)| {
        c.in_default_collection
            && c.attributes.get(ATTR_CONNECTION_ID).map(String::as_str) == Some(connection_id)
            && c.attributes
                .get(ATTR_KIND)
                .is_some_and(|k| kinds.contains(&k.as_str()))
    });
    let (index, _) = matching.next()?;
    matching.next().is_none().then_some(index)
}

impl SecretStore for SecretServiceStore {
    fn kind(&self) -> StoreKind {
        StoreKind::Keyring
    }

    fn get(&self, id: &SecretId) -> Result<SecretString, SecretError> {
        let ss = Self::connect()?;
        let item = self
            .find_for_read(&ss, id)?
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
        let mut ops = SsOps {
            store: self,
            ss: &ss,
            collection: &collection,
            id,
            meta,
            secret,
            found: Vec::new(),
        };
        write_then_prune(&mut ops, id)
    }

    /// Deletes **every** item stored under `id` in this project (stale duplicates included).
    /// See `delete_matches` (private) for the outcome when some deletions fail.
    fn delete(&self, id: &SecretId) -> Result<bool, SecretError> {
        let ss = Self::connect()?;
        let items = self.search(&ss, id)?;
        if items.is_empty() {
            return Ok(false);
        }
        // Without the default collection (no `default` alias, lookup failure) every item is
        // treated as non-default and still deleted: a failed lookup must not keep secrets.
        let in_default: Vec<String> = match ss.get_default_collection().and_then(|c| {
            c.search_items(self.lookup(id))
                .map(|found| found.iter().map(|i| i.item_path.to_string()).collect())
        }) {
            Ok(paths) => paths,
            Err(e) => {
                tracing::warn!(id = %id, error = %map_error(e), "cannot look up the default secret collection; deleting every item under the id");
                Vec::new()
            }
        };
        let flagged: Vec<(&Item<'_>, bool)> = items
            .iter()
            .map(|item| (item, in_default.contains(&item.item_path.to_string())))
            .collect();
        delete_matches(id, &flagged, |item| item.delete().map_err(map_error))
    }
}

/// Deletes every item found under `id`, logging each failure. Returns:
/// - `Ok(false)` when there is no item;
/// - the error of a default-collection item that could not be deleted (the credential NineSQL
///   reads is still there);
/// - otherwise `Ok(true)` if at least one item was deleted, else the first error.
fn delete_matches<P>(
    id: &SecretId,
    items: &[(P, bool)],
    mut delete: impl FnMut(&P) -> Result<(), SecretError>,
) -> Result<bool, SecretError> {
    let mut deleted_any = false;
    let mut default_failure = None;
    let mut first_failure = None;
    for (item, in_default) in items {
        match delete(item) {
            Ok(()) => deleted_any = true,
            Err(e) => {
                tracing::warn!(id = %id, in_default_collection = in_default, error = %e, "could not delete a secret item");
                if *in_default && default_failure.is_none() {
                    default_failure = Some(e);
                } else if first_failure.is_none() {
                    first_failure = Some(e);
                }
            }
        }
    }
    match (default_failure, first_failure) {
        (Some(e), _) => Err(e),
        (None, _) if deleted_any => Ok(true),
        (None, Some(e)) => Err(e),
        (None, None) => Ok(false),
    }
}

/// The item operations [`write_then_prune`] needs, abstracted for testing.
trait ItemOps {
    /// Identity of an item (its D-Bus object path).
    type Path: PartialEq + std::fmt::Debug;
    /// Creates (or replaces, same attributes) the item holding the new secret.
    fn create(&mut self) -> Result<Self::Path, SecretError>;
    /// Every item matching the secret id, in any collection.
    fn matching(&mut self) -> Result<Vec<Self::Path>, SecretError>;
    /// Deletes one item.
    fn delete(&mut self, path: &Self::Path) -> Result<(), SecretError>;
}

/// Writes the new secret **first**, then deletes the other items stored under the same id
/// (stale duplicates with older attributes, other collections). If the write fails, the previous
/// credential is untouched. Failing to delete a stale duplicate (locked collection, dismissed
/// prompt) is logged, not fatal: the new secret is already stored. If the created item is not
/// among the matches, nothing is deleted: the search cannot be trusted to tell the new item from
/// stale ones.
fn write_then_prune<O: ItemOps>(ops: &mut O, id: &SecretId) -> Result<(), SecretError> {
    let created = ops.create()?;
    match ops.matching() {
        Ok(paths) if !paths.contains(&created) => {
            tracing::warn!(id = %id, item = ?created, "the new secret item is not among the items found under its id; stale duplicates are left in place");
        }
        Ok(paths) => {
            for stale in paths.iter().filter(|p| **p != created) {
                if let Err(e) = ops.delete(stale) {
                    tracing::warn!(id = %id, item = ?stale, error = %e, "could not delete a stale duplicate secret item");
                }
            }
        }
        Err(e) => {
            tracing::warn!(id = %id, error = %e, "could not look for stale duplicate secret items");
        }
    }
    Ok(())
}

struct SsOps<'s, 'a> {
    store: &'s SecretServiceStore,
    ss: &'a SecretService<'a>,
    collection: &'a secret_service::blocking::Collection<'a>,
    id: &'s SecretId,
    meta: &'s SecretMeta,
    secret: &'s SecretString,
    /// Items returned by the last [`ItemOps::matching`] call.
    found: Vec<Item<'a>>,
}

impl ItemOps for SsOps<'_, '_> {
    /// D-Bus object path, as text.
    type Path = String;

    fn create(&mut self) -> Result<Self::Path, SecretError> {
        let mut attributes = self.store.lookup(self.id);
        attributes.insert(ATTR_CONNECTION_ID, self.meta.connection_id.as_str());
        attributes.insert(ATTR_KIND, self.meta.kind.as_str());
        let item = self
            .collection
            .create_item(
                &self.meta.label,
                attributes,
                self.secret.expose_secret().as_bytes(),
                true, // replace the item with the same attributes
                "text/plain",
            )
            .map_err(map_error)?;
        Ok(item.item_path.to_string())
    }

    fn matching(&mut self) -> Result<Vec<Self::Path>, SecretError> {
        self.found = self.store.search(self.ss, self.id)?;
        Ok(self.found.iter().map(|i| i.item_path.to_string()).collect())
    }

    fn delete(&mut self, path: &Self::Path) -> Result<(), SecretError> {
        match self.found.iter().find(|i| i.item_path.to_string() == *path) {
            Some(item) => item.delete().map_err(map_error),
            None => Ok(()),
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

    /// Fake item store recording the order of operations.
    struct Fake {
        log: Vec<String>,
        create_fails: bool,
        matching: Result<Vec<u32>, ()>,
        locked: Vec<u32>,
    }

    impl Fake {
        fn new(matching: Vec<u32>) -> Self {
            Self {
                log: Vec::new(),
                create_fails: false,
                matching: Ok(matching),
                locked: Vec::new(),
            }
        }
    }

    impl ItemOps for Fake {
        type Path = u32;
        fn create(&mut self) -> Result<u32, SecretError> {
            self.log.push("create".into());
            if self.create_fails {
                Err(SecretError::Locked(StoreKind::Keyring))
            } else {
                Ok(7)
            }
        }
        fn matching(&mut self) -> Result<Vec<u32>, SecretError> {
            self.log.push("matching".into());
            self.matching
                .clone()
                .map_err(|()| SecretError::Locked(StoreKind::Keyring))
        }
        fn delete(&mut self, path: &u32) -> Result<(), SecretError> {
            self.log.push(format!("delete {path}"));
            if self.locked.contains(path) {
                Err(SecretError::Locked(StoreKind::Keyring))
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn creates_first_then_deletes_only_other_items() {
        let mut fake = Fake::new(vec![3, 7, 5]);
        write_then_prune(&mut fake, &id()).unwrap();
        assert_eq!(fake.log, ["create", "matching", "delete 3", "delete 5"]);
    }

    #[test]
    fn failed_create_deletes_nothing() {
        let mut fake = Fake::new(vec![3]);
        fake.create_fails = true;
        assert!(matches!(
            write_then_prune(&mut fake, &id()),
            Err(SecretError::Locked(_))
        ));
        assert_eq!(fake.log, ["create"]);
    }

    #[test]
    fn stale_duplicate_failures_are_not_fatal() {
        let mut fake = Fake::new(vec![3, 7, 5]);
        fake.locked = vec![3];
        write_then_prune(&mut fake, &id()).unwrap();
        assert_eq!(fake.log, ["create", "matching", "delete 3", "delete 5"]);

        let mut fake = Fake::new(vec![]);
        fake.matching = Err(());
        write_then_prune(&mut fake, &id()).unwrap();
        assert_eq!(fake.log, ["create", "matching"]);
    }

    /// Security review L8: delete removes every match and succeeds unless the default-collection
    /// item stays.
    #[test]
    fn delete_removes_every_match() {
        let id = id();
        let mut seen = Vec::new();
        let items = [(1u32, false), (2, true), (3, false)];
        let ok = delete_matches(&id, &items, |p| {
            seen.push(*p);
            if *p == 3 {
                Err(SecretError::Locked(StoreKind::Keyring))
            } else {
                Ok(())
            }
        });
        assert!(ok.unwrap());
        assert_eq!(seen, [1, 2, 3]);
        // The default-collection item could not be deleted: error, after trying the others.
        let mut seen = Vec::new();
        let err = delete_matches(&id, &items, |p| {
            seen.push(*p);
            if *p == 2 {
                Err(SecretError::Locked(StoreKind::Keyring))
            } else {
                Ok(())
            }
        });
        assert!(matches!(err, Err(SecretError::Locked(_))));
        assert_eq!(seen, [1, 2, 3]);
        // Nothing deleted at all: the first error.
        let err = delete_matches(&id, &[(1u32, false)], |_| {
            Err(SecretError::Locked(StoreKind::Keyring))
        });
        assert!(matches!(err, Err(SecretError::Locked(_))));
        assert!(!delete_matches(&id, &[] as &[(u32, bool)], |_| Ok(())).unwrap());
    }

    #[test]
    fn prune_is_skipped_when_the_created_item_is_not_found() {
        let mut fake = Fake::new(vec![3, 5]);
        write_then_prune(&mut fake, &id()).unwrap();
        assert_eq!(fake.log, ["create", "matching"]);
    }

    const PROJECT: &str = "3F2A6C1E-0B7D-4C51-9E2A-6D8F0A1B2C3D";
    const CONN: &str = "8a6e0f62-6b0c-4d8e-9f1e-2a4b6c8d0e1f";

    fn candidate(in_default: bool, connection_id: &str, kind: &str) -> Candidate {
        Candidate {
            in_default_collection: in_default,
            attributes: HashMap::from([
                (ATTR_CONNECTION_ID.to_owned(), connection_id.to_owned()),
                (ATTR_KIND.to_owned(), kind.to_owned()),
            ]),
        }
    }

    #[test]
    fn duplicates_resolve_to_the_matching_default_collection_item() {
        let id = SecretId::new(format!("{CONN}:auth")).unwrap();
        let other = "00000000-0000-4000-8000-000000000000";
        // A stale copy in another collection and one with a foreign connection id lose.
        let found = [
            candidate(false, CONN, "password"),
            candidate(true, other, "password"),
            candidate(true, CONN, "password"),
        ];
        assert_eq!(pick_duplicate(&id, &found), Some(2));
        // A kind that does not fit the slot loses.
        let found = [
            candidate(true, CONN, "ssh_password"),
            candidate(true, CONN, "token"),
        ];
        assert_eq!(pick_duplicate(&id, &found), Some(1));
        // Two plausible items, or none: ambiguous.
        let found = [
            candidate(true, CONN, "password"),
            candidate(true, CONN, "token"),
        ];
        assert_eq!(pick_duplicate(&id, &found), None);
        let found = [
            candidate(false, CONN, "password"),
            candidate(false, CONN, "password"),
        ];
        assert_eq!(pick_duplicate(&id, &found), None);
        // Unreadable attributes are never picked.
        let found = [Candidate::default(), candidate(true, CONN, "password")];
        assert_eq!(pick_duplicate(&id, &found), Some(1));
        // Jump hosts and other slots.
        let jump = SecretId::new(format!("{CONN}:ssh-jump-2")).unwrap();
        let found = [
            candidate(true, CONN, "password"),
            candidate(true, CONN, "ssh_key_passphrase"),
        ];
        assert_eq!(pick_duplicate(&jump, &found), Some(1));
        let odd = SecretId::new(format!("{CONN}:ssh-jump-x")).unwrap();
        assert_eq!(pick_duplicate(&odd, &found), None);
        assert_eq!(
            pick_duplicate(&SecretId::new("no-slot").unwrap(), &found),
            None
        );
    }

    #[test]
    fn project_must_be_a_uuid() {
        assert!(SecretServiceStore::new(PROJECT).is_ok());
        for bad in [
            "General",
            "",
            "3f2a6c1e0b7d4c519e2a6d8f0a1b2c3d",
            "3f2a6c1e-0b7d-4c51-9e2a-6d8f0a1b2c3",
            "3f2a6c1e-0b7d-4c51-9e2a-6d8f0a1b2c3g",
            "3f2a6c1e-0b7d-4c51-9e2a-6d8f0a1b2c3d-",
        ] {
            assert!(SecretServiceStore::new(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn lookup_attributes() {
        let id = id();
        let store = SecretServiceStore::new(PROJECT).unwrap();
        let attrs = store.lookup(&id);
        assert_eq!(attrs.get("xdg:schema"), Some(&"org.ninesql.Secret"));
        assert_eq!(attrs.get("service"), Some(&"ninesql"));
        // The project UUID, lower-cased, never a project name.
        assert_eq!(
            attrs.get("project"),
            Some(&"3f2a6c1e-0b7d-4c51-9e2a-6d8f0a1b2c3d")
        );
        assert_eq!(attrs.get("secret_id"), Some(&"spike:password"));
        assert_eq!(attrs.len(), 4);
    }

    /// Round trip against a real Secret Service. Ignored by default: run it on a desktop session
    /// with `cargo test -p ninesql-secrets --features secret-service -- --ignored`.
    #[test]
    #[ignore = "needs a session bus with a Secret Service (GNOME Keyring, KWallet, KeePassXC)"]
    fn round_trip_on_a_real_secret_service() {
        let store = SecretServiceStore::new("00000000-0000-4000-8000-00000000c0de").unwrap();
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
    fn probe_timeout_panic_and_success_paths() {
        use crate::backend::SecretServiceAvailability as A;
        let slow = probe_with(
            || {
                std::thread::sleep(Duration::from_secs(2));
                A::Available
            },
            Duration::from_millis(50),
        );
        assert_eq!(
            slow,
            A::Unavailable {
                detail: "timed out".into()
            }
        );
        let panicked = probe_with(|| panic!("probe panics"), Duration::from_secs(5));
        assert!(matches!(panicked, A::Unavailable { .. }), "{panicked:?}");
        assert_eq!(
            probe_with(|| A::Available, Duration::from_secs(5)),
            A::Available
        );
    }

    #[test]
    fn reports_unavailable_without_provider() {
        if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none()
            || std::env::var_os("NINESQL_TEST_EXPECT_NO_SECRET_SERVICE").is_none()
        {
            return;
        }
        let err = SecretServiceStore::new(PROJECT)
            .unwrap()
            .get(&id())
            .unwrap_err();
        assert!(matches!(err, SecretError::Unavailable { .. }), "{err:?}");
    }
}
