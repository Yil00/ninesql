//! Encrypted vault: the fallback secret store when no Secret Service answers (ADR-0005 §2,
//! F-CON-04). ROADMAP: P1-D.
//!
//! One file, `$XDG_DATA_HOME/ninesql/vault.bin` (mode `0600`, written atomically through
//! [`ninesql_storage::fs::write_atomic`], only while the workspace lock is held), holds every
//! secret of every project, each keyed by the project UUID and its [`SecretId`] (ADR-0017 §4).
//! The format is specified in [`format`](self#file-format).
//!
//! # Cryptography
//! - Key: **Argon2id** (version 0x13) of the master password with a 16-byte random salt, 32-byte
//!   output. Parameters are stored in the header; the default is ADR-0005's m = 64 MiB, t = 3,
//!   p = 1 ([`KdfParams::DEFAULT`]). New vaults and password changes accept m from 19 MiB (the
//!   OWASP floor for Argon2id) to 1 GiB, t from 2 to 16, p from 1 to 8 ([`KdfParams::new`]).
//!   Reading a file accepts any Argon2-valid parameters under the same ceilings, so a planted
//!   header cannot make NineSQL allocate more than 1 GiB or loop for long.
//! - Sealing: **XChaCha20-Poly1305** over the whole serialized entry list, with a random 24-byte
//!   nonce renewed on every write and the 112-byte header as associated data: the header (KDF
//!   parameters, salt, lengths) and the payload are authenticated together.
//! - A key-check tag in the header separates a wrong master password from a damaged file.
//!
//! # Memory
//! - The master password is only borrowed as [`SecretString`].
//! - The derived key is written by Argon2 straight into a heap allocation
//!   (`Box<Zeroizing<[u8; 32]>>`) made before the derivation, and only that box moves
//!   afterwards, so moving the unlocked state never copies the key around the stack. It is
//!   zeroized when the vault locks or is dropped.
//! - Each encryption or decryption builds a short-lived `XChaCha20Poly1305` instance that holds
//!   a copy of the key and zeroizes it on drop (`chacha20poly1305` feature `zeroize`). That
//!   instance lives on the stack and Rust may move it (for example when a function returns it),
//!   which can leave unwiped copies in stack memory until later calls overwrite them. Safe Rust
//!   gives no way to build it in place; the window is one call.
//! - The Argon2 working memory (64 MiB by default) is allocated here, fallibly (an allocation
//!   failure is [`VaultError::OutOfMemory`], not an abort), and zeroized after each derivation
//!   (the `argon2` crate does not wipe the memory it allocates itself).
//! - Upstream report items for `argon2` 0.6: besides not wiping its own block memory
//!   (`fill_blocks`, worked around above), its final `blake2b_long` step leaves a 64-byte copy
//!   whose first 32 bytes are the derived key in its own stack frame, which NineSQL cannot
//!   reach to wipe.
//! - Plaintext payload buffers are [`Zeroizing`] and allocated with their final capacity, so
//!   encoding and in-place encryption never reallocate and leave a copy behind.
//! - While unlocked, the key and the decrypted entries ([`SecretString`]) stay in memory;
//!   [`Vault::lock`], the idle timeout and drop zeroize them.
//!
//! # Lifecycle
//! [`Vault::open`] takes the workspace lock (shared with the rest of the process, refused if
//! another NineSQL holds it) and starts **locked**. Then [`Vault::create`] (first use) or
//! [`Vault::unlock`] with the master password; [`Vault::lock`] or the idle timeout
//! ([`Vault::set_idle_timeout`], enforced at every access and by [`Vault::lock_if_idle`], which the
//! core calls from a timer) lock it again. Idle time is measured on `CLOCK_BOOTTIME`, so time
//! spent suspended counts: a vault left unlocked before an overnight suspend is locked at the
//! first check after resume. [`Vault`] is also a [`crate::LockHook`]: registered in a
//! [`crate::LockHooks`], it is locked on logind's `PrepareForSleep(true)` and on the session
//! `Lock` signal ([`crate::lock_events`]), so it is already locked when the machine wakes up. [`Vault::change_master_password`] re-encrypts
//! atomically under a new salt. Failed attempts are slowed down by [`UnlockBackoff`].
//!
//! Reading never writes: a wrong password, a damaged, truncated or newer file is reported with a
//! typed [`VaultError`] and the file is left untouched.
//!
//! # Not covered
//! - Offline guessing of a copied `vault.bin` is slowed only by Argon2id: the master password
//!   must be strong.
//! - Rollback: someone able to write `vault.bin` can put back an older copy (an older set of
//!   secrets under an older master password). Detecting it would need state outside the file.
//! - Calls are blocking, and unlocking runs Argon2id (about a second at the default cost) while
//!   holding the vault's mutex; run them on a blocking thread, never on the GPUI foreground.
//!
//! # File format
//! Version 1, documented in detail in the `format` module source:
//!
//! ```text
//! 0   magic "NSQVAULT" | 8 version u16 LE | 10 KDF id (1 = Argon2id v0x13) | 11 AEAD id
//!     (1 = XChaCha20-Poly1305) | 12 m KiB u32 | 16 t u32 | 20 p u32 | 24 salt[16]
//! 40  key-check nonce[24] | 64 key-check tag[16] | 80 payload nonce[24] | 104 ciphertext len u64
//! 112 ciphertext (payload + 16-byte tag), associated data = bytes 0..112
//! ```

mod format;
#[cfg(test)]
mod tests;
mod throttle;

use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use argon2::{Algorithm, Argon2, Block, Version};
use chacha20poly1305::aead::{AeadInOut, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use ninesql_storage::fs::{ensure_private_dir, read_private, write_atomic};
use ninesql_storage::{DirLock, Paths, StorageError};
use secrecy::{ExposeSecret, SecretString};
use zeroize::Zeroizing;

pub use format::{MAX_CONNECTION_ID_LEN, MAX_ENTRIES, MAX_FILE_LEN, MAX_LABEL_LEN, MAX_SECRET_LEN};
pub use throttle::UnlockBackoff;

use crate::clock::{self, BootInstant, SharedClock};
use crate::error::SecretError;
use crate::project::{InvalidProjectId, normalize_project_id};
use crate::reference::{SecretId, SecretKind, StoreKind};
use crate::store::{SecretMeta, SecretStore};
use format::{
    EntryRef, FORMAT_VERSION, FormatError, HEADER_LEN, Header, KDF_PREFIX_LEN, KEY_CHECK_CONTEXT,
    KEY_LEN, NONCE_LEN, SALT_LEN, TAG_LEN,
};
use throttle::UnlockThrottle;

/// Minimum length, in characters (Unicode scalar values), of a new master password: the vault
/// file is protected by the master password alone once copied off the machine (docs/05,
/// "Vault format v1"). Checked when a vault is created or its password changed, never on unlock.
pub const MIN_MASTER_PASSWORD_CHARS: usize = 12;

/// Checks a new master password against the minimum length.
fn check_new_master(master: &SecretString) -> Result<(), VaultError> {
    let master = master.expose_secret();
    let chars = master.chars().count();
    if master.chars().all(char::is_whitespace) {
        Err(VaultError::EmptyPassword)
    } else if chars < MIN_MASTER_PASSWORD_CHARS {
        Err(VaultError::PasswordTooShort {
            min: MIN_MASTER_PASSWORD_CHARS,
        })
    } else {
        Ok(())
    }
}

/// Error of a vault operation. Messages are user-presentable and never contain a secret, the
/// master password, or file content.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum VaultError {
    /// No `vault.bin` yet: create it with a master password first.
    #[error("the vault does not exist yet; create it with a master password")]
    NotCreated,
    /// [`Vault::create`] found an existing vault.
    #[error("a vault already exists at {0}")]
    AlreadyExists(PathBuf),
    /// The vault is locked (at startup, after [`Vault::lock`] or after the idle timeout).
    #[error("the vault is locked; unlock it with the master password")]
    Locked,
    /// The master password does not open the vault (or the part of the header it is checked
    /// against was damaged: the two cannot be told apart without a new oracle).
    #[error("wrong master password, or the vault header is damaged")]
    WrongPassword,
    /// The file is not a vault, is truncated or extended, or its content was modified. It was
    /// left untouched.
    #[error("the vault file {path} is damaged or was modified ({detail}); it was left untouched")]
    Corrupted {
        /// The vault file.
        path: PathBuf,
        /// What is wrong, a fixed description (never file content).
        detail: &'static str,
    },
    /// The file was written by a newer NineSQL. It was left untouched.
    #[error(
        "the vault file {path} uses format version {found}; this NineSQL reads up to version {supported}. Update NineSQL to open it"
    )]
    NewerVersion {
        /// The vault file.
        path: PathBuf,
        /// Version found.
        found: u16,
        /// Newest version this build reads.
        supported: u16,
    },
    /// Too many failed attempts: wait before trying again ([`UnlockBackoff`]).
    #[error("too many failed attempts; try again in {} s", retry_after.as_secs().max(1))]
    TooManyAttempts {
        /// Time left before the next attempt is accepted.
        retry_after: Duration,
    },
    /// Argon2id parameters outside the accepted range.
    #[error("invalid key derivation parameters: {0}")]
    InvalidKdfParams(&'static str),
    /// An empty master password, or one made only of whitespace, was given to create or
    /// change the vault.
    #[error("the master password must not be empty or only spaces")]
    EmptyPassword,
    /// A new master password shorter than [`MIN_MASTER_PASSWORD_CHARS`] (vault creation or
    /// password change; an existing vault still unlocks with its password, whatever its length).
    #[error("the master password must be at least {min} characters long")]
    PasswordTooShort {
        /// Minimum number of characters.
        min: usize,
    },
    /// A secret, label, connection id or the whole vault exceeds a size limit.
    #[error("vault limit exceeded: {0}")]
    Limit(&'static str),
    /// Not enough memory for the Argon2id working memory.
    #[error("not enough memory for the vault key derivation ({0} KiB)")]
    OutOfMemory(u32),
    /// The system random number generator failed.
    #[error("the system random number generator failed")]
    Random,
    /// Encryption failed (not expected with valid inputs).
    #[error("vault encryption failed")]
    Crypto,
    /// File system or workspace lock error (another NineSQL holds the lock, permissions, I/O).
    #[error(transparent)]
    Storage(#[from] StorageError),
}

/// State of the vault.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VaultStatus {
    /// No `vault.bin` yet.
    NotCreated,
    /// The file exists; the master password is needed.
    Locked,
    /// Open: secrets can be read and written.
    Unlocked,
}

/// Argon2id parameters of a vault (stored in its header).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KdfParams {
    m_cost_kib: u32,
    t_cost: u32,
    p_cost: u32,
}

impl Default for KdfParams {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl KdfParams {
    /// ADR-0005 §2: m = 64 MiB, t = 3, p = 1.
    pub const DEFAULT: Self = Self {
        m_cost_kib: 64 * 1024,
        t_cost: 3,
        p_cost: 1,
    };
    /// Smallest memory cost accepted for a new vault (19 MiB, OWASP minimum for Argon2id).
    pub const MIN_M_COST_KIB: u32 = 19 * 1024;
    /// Largest memory cost accepted, also when reading a file (1 GiB).
    pub const MAX_M_COST_KIB: u32 = 1024 * 1024;
    /// Smallest time cost accepted for a new vault.
    pub const MIN_T_COST: u32 = 2;
    /// Largest time cost accepted, also when reading a file.
    pub const MAX_T_COST: u32 = 16;
    /// Largest parallelism accepted, also when reading a file.
    pub const MAX_P_COST: u32 = 8;

    /// Parameters for a new vault or a password change.
    ///
    /// # Errors
    /// [`VaultError::InvalidKdfParams`] outside `MIN_*..=MAX_*`, or if `p` is 0.
    pub fn new(m_cost_kib: u32, t_cost: u32, p_cost: u32) -> Result<Self, VaultError> {
        if !(Self::MIN_M_COST_KIB..=Self::MAX_M_COST_KIB).contains(&m_cost_kib) {
            return Err(VaultError::InvalidKdfParams("memory cost out of range"));
        }
        if !(Self::MIN_T_COST..=Self::MAX_T_COST).contains(&t_cost) {
            return Err(VaultError::InvalidKdfParams("time cost out of range"));
        }
        if !(1..=Self::MAX_P_COST).contains(&p_cost) {
            return Err(VaultError::InvalidKdfParams("parallelism out of range"));
        }
        Self::from_file(m_cost_kib, t_cost, p_cost).ok_or(VaultError::InvalidKdfParams(
            "memory cost too low for the parallelism",
        ))
    }

    /// Very cheap parameters (8 KiB, one pass) that make vault tests fast. **Insecure**: only
    /// compiled for tests and with the test-only `memory-store` feature.
    #[cfg(any(test, feature = "memory-store"))]
    #[must_use]
    pub const fn insecure_for_tests() -> Self {
        Self {
            m_cost_kib: 8,
            t_cost: 1,
            p_cost: 1,
        }
    }

    /// Parameters read from a file: any Argon2-valid value under the ceilings.
    pub(crate) fn from_file(m_cost_kib: u32, t_cost: u32, p_cost: u32) -> Option<Self> {
        let valid = (1..=Self::MAX_P_COST).contains(&p_cost)
            && (1..=Self::MAX_T_COST).contains(&t_cost)
            && m_cost_kib <= Self::MAX_M_COST_KIB
            && m_cost_kib >= 8 * p_cost
            && argon2::Params::new(m_cost_kib, t_cost, p_cost, Some(KEY_LEN)).is_ok();
        valid.then_some(Self {
            m_cost_kib,
            t_cost,
            p_cost,
        })
    }

    /// Whether these parameters are within the bounds accepted for a new vault
    /// ([`KdfParams::new`]). A vault read from disk may have weaker ones (written by a test build
    /// or by hand); [`Vault::unlock`] warns about it and [`Vault::change_master_password`] without
    /// explicit parameters then moves to [`KdfParams::DEFAULT`].
    #[must_use]
    pub fn meets_minimum(&self) -> bool {
        Self::new(self.m_cost_kib, self.t_cost, self.p_cost).is_ok()
    }

    /// Memory cost in KiB.
    #[must_use]
    pub const fn m_cost_kib(&self) -> u32 {
        self.m_cost_kib
    }

    /// Number of passes.
    #[must_use]
    pub const fn t_cost(&self) -> u32 {
        self.t_cost
    }

    /// Number of lanes.
    #[must_use]
    pub const fn p_cost(&self) -> u32 {
        self.p_cost
    }
}

/// The encrypted vault file and its unlocked state. Share it as `Arc<Vault>`; give each project a
/// [`VaultStore`].
pub struct Vault {
    path: PathBuf,
    // Held for the lifetime of the vault: no other NineSQL process writes `vault.bin` meanwhile.
    _lock: Arc<DirLock>,
    state: Mutex<State>,
    clock: SharedClock,
}

struct State {
    unlocked: Option<Unlocked>,
    throttle: UnlockThrottle,
    idle_timeout: Option<Duration>,
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct EntryKey {
    /// Project UUID, lower case.
    project: String,
    id: SecretId,
}

struct Entry {
    connection_id: String,
    kind: SecretKind,
    label: String,
    secret: SecretString,
}

/// Derived vault key: on the heap from before the derivation, zeroized on drop.
type Key = Box<Zeroizing<[u8; KEY_LEN]>>;

/// Key and decrypted entries of an open vault. Dropping it zeroizes the key and every secret.
struct Unlocked {
    key: Key,
    kdf: KdfParams,
    salt: [u8; SALT_LEN],
    check_nonce: [u8; NONCE_LEN],
    check_tag: [u8; TAG_LEN],
    entries: BTreeMap<EntryKey, Entry>,
    last_used: BootInstant,
}

impl fmt::Debug for Vault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let unlocked = self.state.try_lock().ok().map(|s| s.unlocked.is_some());
        f.debug_struct("Vault")
            .field("path", &self.path)
            .field("unlocked", &unlocked)
            .finish_non_exhaustive()
    }
}

impl Vault {
    /// Opens the workspace vault, `$XDG_DATA_HOME/ninesql/vault.bin`, locked. Creates the data
    /// directory (`0700`) if needed and takes the workspace lock `ninesql.lock` (the same lock as
    /// the rest of this process; refused if another NineSQL process holds it).
    ///
    /// # Errors
    /// [`VaultError::Storage`] with [`StorageError::Locked`] when another NineSQL process uses the
    /// workspace, [`StorageError::LockUnsupported`], or an I/O / ownership error.
    pub fn open(paths: &Paths) -> Result<Self, VaultError> {
        ensure_private_dir(paths.data_dir())?;
        Self::open_at(paths.vault_file(), &paths.lock_file())
    }

    /// [`Vault::open`] with explicit paths: the vault file and the lock file (both directories
    /// must exist).
    ///
    /// # Errors
    /// See [`Vault::open`].
    pub fn open_at(vault_file: impl Into<PathBuf>, lock_file: &Path) -> Result<Self, VaultError> {
        Self::open_at_with_clock(vault_file, lock_file, clock::system())
    }

    fn open_at_with_clock(
        vault_file: impl Into<PathBuf>,
        lock_file: &Path,
        clock: SharedClock,
    ) -> Result<Self, VaultError> {
        let lock = DirLock::acquire(lock_file)?;
        Ok(Self {
            path: vault_file.into(),
            _lock: lock,
            state: Mutex::new(State {
                unlocked: None,
                throttle: UnlockThrottle::new(UnlockBackoff::default()),
                idle_timeout: None,
            }),
            clock,
        })
    }

    /// The vault file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn state(&self) -> MutexGuard<'_, State> {
        // A panic while holding the lock leaves at worst an in-memory entry that the file does
        // not have; recover rather than make the vault unusable.
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn exists(&self) -> Result<bool, VaultError> {
        match std::fs::symlink_metadata(&self.path) {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(StorageError::Io {
                path: self.path.clone(),
                source: e,
            }
            .into()),
        }
    }

    /// Current state (applies the idle timeout first).
    ///
    /// # Errors
    /// An I/O error while checking whether the file exists.
    pub fn status(&self) -> Result<VaultStatus, VaultError> {
        let mut st = self.state();
        expire_if_idle(&mut st, self.clock.now());
        if st.unlocked.is_some() {
            Ok(VaultStatus::Unlocked)
        } else if self.exists()? {
            Ok(VaultStatus::Locked)
        } else {
            Ok(VaultStatus::NotCreated)
        }
    }

    /// Whether the vault is unlocked (applies the idle timeout first).
    #[must_use]
    pub fn is_unlocked(&self) -> bool {
        let mut st = self.state();
        expire_if_idle(&mut st, self.clock.now());
        st.unlocked.is_some()
    }

    /// Creates an empty vault protected by `master` and leaves it unlocked.
    ///
    /// # Errors
    /// [`VaultError::EmptyPassword`], [`VaultError::PasswordTooShort`] (fewer than
    /// [`MIN_MASTER_PASSWORD_CHARS`] characters), [`VaultError::AlreadyExists`], storage errors.
    pub fn create(&self, master: &SecretString, params: KdfParams) -> Result<(), VaultError> {
        check_new_master(master)?;
        let mut st = self.state();
        if self.exists()? {
            return Err(VaultError::AlreadyExists(self.path.clone()));
        }
        let unlocked = Unlocked::fresh(master, params, BTreeMap::new(), self.clock.now())?;
        unlocked.persist(&self.path)?;
        tracing::info!(path = %self.path.display(), m_cost_kib = params.m_cost_kib, t_cost = params.t_cost, p_cost = params.p_cost, "vault created");
        st.unlocked = Some(unlocked);
        st.throttle.record_success();
        Ok(())
    }

    /// Unlocks the vault with its master password. Never modifies the file.
    ///
    /// # Errors
    /// [`VaultError::TooManyAttempts`] (checked before any work), [`VaultError::NotCreated`],
    /// [`VaultError::WrongPassword`] (counted by the back-off), [`VaultError::Corrupted`],
    /// [`VaultError::NewerVersion`], storage errors.
    pub fn unlock(&self, master: &SecretString) -> Result<(), VaultError> {
        let mut st = self.state();
        st.throttle
            .check(self.clock.now())
            .map_err(|retry_after| VaultError::TooManyAttempts { retry_after })?;
        let file = self.read_file()?;
        match self.open_with(&file, master) {
            Ok(unlocked) => {
                tracing::info!(path = %self.path.display(), entries = unlocked.entries.len(), "vault unlocked");
                if !unlocked.kdf.meets_minimum() {
                    tracing::warn!(
                        path = %self.path.display(),
                        m_cost_kib = unlocked.kdf.m_cost_kib,
                        t_cost = unlocked.kdf.t_cost,
                        p_cost = unlocked.kdf.p_cost,
                        "the vault uses key derivation parameters below the accepted minimum; change the master password to upgrade them"
                    );
                }
                st.unlocked = Some(unlocked);
                st.throttle.record_success();
                Ok(())
            }
            Err(VaultError::WrongPassword) => {
                st.throttle.record_failure(self.clock.now());
                tracing::warn!(path = %self.path.display(), "vault unlock failed: wrong master password or damaged header");
                Err(VaultError::WrongPassword)
            }
            Err(e) => {
                tracing::warn!(path = %self.path.display(), error = %e, "vault unlock failed");
                Err(e)
            }
        }
    }

    /// Locks the vault: the key and every decrypted secret are dropped and zeroized.
    pub fn lock(&self) {
        if self.state().unlocked.take().is_some() {
            tracing::debug!(path = %self.path.display(), "vault locked");
        }
    }

    /// Locks the vault automatically after `timeout` without use (`None`: never). The timeout is
    /// checked at every access; call [`Vault::lock_if_idle`] from a timer so the secrets also
    /// leave memory when nothing touches the vault.
    pub fn set_idle_timeout(&self, timeout: Option<Duration>) {
        self.state().idle_timeout = timeout;
    }

    /// Locks the vault if it has been idle for longer than the idle timeout. Returns `true` if it
    /// locked it now.
    pub fn lock_if_idle(&self) -> bool {
        expire_if_idle(&mut self.state(), self.clock.now())
    }

    /// Replaces the back-off policy for failed attempts.
    pub fn set_unlock_backoff(&self, policy: UnlockBackoff) {
        self.state().throttle.set_policy(policy);
    }

    /// Argon2id parameters of the unlocked vault.
    #[must_use]
    pub fn kdf_params(&self) -> Option<KdfParams> {
        self.state().unlocked.as_ref().map(|u| u.kdf)
    }

    /// Re-encrypts the vault under `new` with a new salt and `params`, atomically. With `None`,
    /// the current parameters are kept if they meet [`KdfParams::meets_minimum`], otherwise
    /// [`KdfParams::DEFAULT`] is used: on any error the previous file stays in place. The current
    /// password is checked against the file even when the vault is unlocked. Leaves the vault
    /// unlocked.
    ///
    /// # Errors
    /// [`VaultError::EmptyPassword`], [`VaultError::PasswordTooShort`] for `new`,
    /// [`VaultError::TooManyAttempts`],
    /// [`VaultError::WrongPassword`] for `current`, and the errors of [`Vault::unlock`].
    pub fn change_master_password(
        &self,
        current: &SecretString,
        new: &SecretString,
        params: Option<KdfParams>,
    ) -> Result<(), VaultError> {
        check_new_master(new)?;
        let mut st = self.state();
        st.throttle
            .check(self.clock.now())
            .map_err(|retry_after| VaultError::TooManyAttempts { retry_after })?;
        let file = self.read_file()?;
        let old = match self.open_with(&file, current) {
            Ok(old) => old,
            Err(VaultError::WrongPassword) => {
                st.throttle.record_failure(self.clock.now());
                tracing::warn!(path = %self.path.display(), "vault password change refused: wrong master password");
                return Err(VaultError::WrongPassword);
            }
            Err(e) => return Err(e),
        };
        st.throttle.record_success();
        let kdf = params.unwrap_or(if old.kdf.meets_minimum() {
            old.kdf
        } else {
            KdfParams::DEFAULT
        });
        let Unlocked { entries, .. } = old;
        let fresh = Unlocked::fresh(new, kdf, entries, self.clock.now())?;
        fresh.persist(&self.path)?;
        tracing::info!(path = %self.path.display(), m_cost_kib = kdf.m_cost_kib, t_cost = kdf.t_cost, p_cost = kdf.p_cost, "vault master password changed");
        st.unlocked = Some(fresh);
        Ok(())
    }

    fn read_file(&self) -> Result<Vec<u8>, VaultError> {
        match read_private(&self.path, MAX_FILE_LEN) {
            Ok(Some(bytes)) => Ok(bytes),
            Ok(None) => Err(VaultError::NotCreated),
            Err(StorageError::TooLarge { .. }) => Err(self.corrupted("file too large")),
            Err(e) => Err(e.into()),
        }
    }

    fn corrupted(&self, detail: &'static str) -> VaultError {
        VaultError::Corrupted {
            path: self.path.clone(),
            detail,
        }
    }

    fn format_error(&self, e: FormatError) -> VaultError {
        match e {
            FormatError::NotAVault => self.corrupted("not a NineSQL vault file"),
            FormatError::NewerVersion(found) => VaultError::NewerVersion {
                path: self.path.clone(),
                found,
                supported: FORMAT_VERSION,
            },
            FormatError::Invalid(detail) => self.corrupted(detail),
        }
    }

    /// Decrypts a whole file with `master`.
    fn open_with(&self, file: &[u8], master: &SecretString) -> Result<Unlocked, VaultError> {
        let (header, ciphertext) = Header::decode(file).map_err(|e| self.format_error(e))?;
        let key = derive_key(master, &header.salt, header.kdf)?;
        let cipher = cipher(&key);
        if !verify_key_check(
            &cipher,
            &header.check_nonce,
            &header.check_tag,
            &file[..KDF_PREFIX_LEN],
        ) {
            return Err(VaultError::WrongPassword);
        }
        let mut plain = Zeroizing::new(ciphertext.to_vec());
        cipher
            .decrypt_in_place(
                &XNonce::from(header.nonce),
                &file[..HEADER_LEN],
                &mut *plain,
            )
            .map_err(|_| self.corrupted("payload authentication failed"))?;
        let decoded = format::decode_payload(&plain).map_err(|d| self.corrupted(d))?;
        let mut entries = BTreeMap::new();
        for d in decoded {
            let project = normalize_project_id(d.project)
                .map_err(|_| self.corrupted("invalid project id"))?;
            let id = SecretId::new(d.id).map_err(|_| self.corrupted("invalid secret id"))?;
            let entry = Entry {
                connection_id: d.connection_id.to_owned(),
                kind: d.kind,
                label: d.label.to_owned(),
                // Exact-capacity copy: boxing into `SecretString` does not reallocate.
                secret: SecretString::from(d.secret.to_owned()),
            };
            if entries.insert(EntryKey { project, id }, entry).is_some() {
                return Err(self.corrupted("duplicate entry"));
            }
        }
        Ok(Unlocked {
            key,
            kdf: header.kdf,
            salt: header.salt,
            check_nonce: header.check_nonce,
            check_tag: header.check_tag,
            entries,
            last_used: self.clock.now(),
        })
    }

    /// Runs `f` on the unlocked state, or fails with [`VaultError::Locked`] /
    /// [`VaultError::NotCreated`].
    fn with_unlocked<R>(
        &self,
        f: impl FnOnce(&mut Unlocked, &Path) -> Result<R, VaultError>,
    ) -> Result<R, VaultError> {
        let mut st = self.state();
        let now = self.clock.now();
        expire_if_idle(&mut st, now);
        let Some(unlocked) = st.unlocked.as_mut() else {
            return Err(if self.exists()? {
                VaultError::Locked
            } else {
                VaultError::NotCreated
            });
        };
        let result = f(unlocked, &self.path)?;
        unlocked.last_used = now;
        Ok(result)
    }

    fn get(&self, project: &str, id: &SecretId) -> Result<Option<SecretString>, VaultError> {
        self.with_unlocked(|u, _| {
            Ok(u.entries
                .get(&EntryKey {
                    project: project.to_owned(),
                    id: id.clone(),
                })
                .map(|e| SecretString::from(e.secret.expose_secret().to_owned())))
        })
    }

    fn set(
        &self,
        project: &str,
        id: &SecretId,
        meta: &SecretMeta,
        secret: &SecretString,
    ) -> Result<(), VaultError> {
        if secret.expose_secret().len() > MAX_SECRET_LEN {
            return Err(VaultError::Limit("secret longer than 64 KiB"));
        }
        if meta.connection_id.len() > MAX_CONNECTION_ID_LEN {
            return Err(VaultError::Limit("connection id longer than 256 bytes"));
        }
        if meta.label.len() > MAX_LABEL_LEN {
            return Err(VaultError::Limit("label longer than 1024 bytes"));
        }
        let entry = Entry {
            connection_id: meta.connection_id.clone(),
            kind: meta.kind,
            label: meta.label.clone(),
            secret: SecretString::from(secret.expose_secret().to_owned()),
        };
        let key = EntryKey {
            project: project.to_owned(),
            id: id.clone(),
        };
        self.with_unlocked(|u, path| {
            if !u.entries.contains_key(&key) && u.entries.len() >= MAX_ENTRIES {
                return Err(VaultError::Limit("too many secrets in the vault"));
            }
            let previous = u.entries.insert(key.clone(), entry);
            if let Err(e) = u.persist(path) {
                // Keep memory identical to the file, which is unchanged.
                match previous {
                    Some(p) => u.entries.insert(key, p),
                    None => u.entries.remove(&key),
                };
                return Err(e);
            }
            Ok(())
        })
    }

    fn delete(&self, project: &str, id: &SecretId) -> Result<bool, VaultError> {
        let key = EntryKey {
            project: project.to_owned(),
            id: id.clone(),
        };
        self.with_unlocked(|u, path| {
            let Some(previous) = u.entries.remove(&key) else {
                return Ok(false);
            };
            if let Err(e) = u.persist(path) {
                u.entries.insert(key, previous);
                return Err(e);
            }
            Ok(true)
        })
    }

    fn ids(&self, project: &str) -> Result<Vec<SecretId>, VaultError> {
        self.with_unlocked(|u, _| {
            Ok(u.entries
                .keys()
                .filter(|k| k.project == project)
                .map(|k| k.id.clone())
                .collect())
        })
    }
}

/// Locks `st` if its idle timeout has passed. Returns `true` if it locked it.
fn expire_if_idle(st: &mut State, now: BootInstant) -> bool {
    let expired = match (&st.unlocked, st.idle_timeout) {
        (Some(u), Some(timeout)) => now.saturating_duration_since(u.last_used) >= timeout,
        _ => false,
    };
    if expired {
        st.unlocked = None;
        tracing::debug!("vault locked after the idle timeout");
    }
    expired
}

impl Unlocked {
    /// New key material (salt, key, key check) for `master`, holding `entries`.
    fn fresh(
        master: &SecretString,
        kdf: KdfParams,
        entries: BTreeMap<EntryKey, Entry>,
        now: BootInstant,
    ) -> Result<Self, VaultError> {
        let mut salt = [0u8; SALT_LEN];
        random(&mut salt)?;
        let mut check_nonce = [0u8; NONCE_LEN];
        random(&mut check_nonce)?;
        let key = derive_key(master, &salt, kdf)?;
        let prefix = Header {
            kdf,
            salt,
            check_nonce,
            check_tag: [0; TAG_LEN],
            nonce: [0; NONCE_LEN],
            ciphertext_len: 0,
        }
        .encode();
        let check_tag = key_check_tag(&cipher(&key), &check_nonce, &prefix[..KDF_PREFIX_LEN])?;
        Ok(Self {
            key,
            kdf,
            salt,
            check_nonce,
            check_tag,
            entries,
            last_used: now,
        })
    }

    /// The whole file: header and sealed payload, with a fresh payload nonce.
    fn seal(&self) -> Result<Vec<u8>, VaultError> {
        let refs: Vec<EntryRef<'_>> = self
            .entries
            .iter()
            .map(|(k, e)| EntryRef {
                project: &k.project,
                id: k.id.as_str(),
                kind: e.kind,
                connection_id: &e.connection_id,
                label: &e.label,
                secret: e.secret.expose_secret().as_bytes(),
            })
            .collect();
        let len = format::payload_len(refs.iter());
        let total = HEADER_LEN + len + TAG_LEN;
        if total as u64 > MAX_FILE_LEN {
            return Err(VaultError::Limit("the vault would exceed 16 MiB"));
        }
        let mut buf = Zeroizing::new(Vec::with_capacity(len + TAG_LEN));
        format::encode_payload(&refs, &mut buf).map_err(VaultError::Limit)?;
        let mut nonce = [0u8; NONCE_LEN];
        random(&mut nonce)?;
        let header = Header {
            kdf: self.kdf,
            salt: self.salt,
            check_nonce: self.check_nonce,
            check_tag: self.check_tag,
            nonce,
            ciphertext_len: (len + TAG_LEN) as u64,
        }
        .encode();
        cipher(&self.key)
            .encrypt_in_place(&XNonce::from(nonce), &header, &mut *buf)
            .map_err(|_| VaultError::Crypto)?;
        let mut file = Vec::with_capacity(total);
        file.extend_from_slice(&header);
        file.extend_from_slice(&buf);
        Ok(file)
    }

    /// Seals and writes the file atomically (`0600`, temp file + rename).
    fn persist(&self, path: &Path) -> Result<(), VaultError> {
        let file = self.seal()?;
        write_atomic(path, &file)?;
        tracing::debug!(path = %path.display(), entries = self.entries.len(), "vault written");
        Ok(())
    }
}

fn random(out: &mut [u8]) -> Result<(), VaultError> {
    getrandom::fill(out).map_err(|_| VaultError::Random)
}

fn cipher(key: &[u8; KEY_LEN]) -> XChaCha20Poly1305 {
    XChaCha20Poly1305::new(key.into())
}

/// Argon2id of `master` into a zeroizing key allocated on the heap beforehand, with working
/// memory allocated fallibly and zeroized afterwards.
fn derive_key(
    master: &SecretString,
    salt: &[u8; SALT_LEN],
    kdf: KdfParams,
) -> Result<Key, VaultError> {
    let params = argon2::Params::new(kdf.m_cost_kib, kdf.t_cost, kdf.p_cost, Some(KEY_LEN))
        .map_err(|_| VaultError::InvalidKdfParams("rejected by Argon2"))?;
    let blocks = params.block_count();
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut memory: Zeroizing<Vec<Block>> = Zeroizing::new(Vec::new());
    memory
        .try_reserve_exact(blocks)
        .map_err(|_| VaultError::OutOfMemory(kdf.m_cost_kib))?;
    memory.resize(blocks, Block::default());
    // Allocated (zero) before the derivation writes into it: NineSQL's code never holds the key
    // on the stack. Upstream caveat (argon2 0.6, to report): its final `blake2b_long` step
    // leaves a 64-byte buffer whose first 32 bytes are the key in its own stack frame, unwiped.
    let mut key: Key = Box::new(Zeroizing::new([0u8; KEY_LEN]));
    argon
        .hash_password_into_with_memory(
            master.expose_secret().as_bytes(),
            salt,
            key.as_mut_slice(),
            memory.as_mut_slice(),
        )
        .map_err(|_| VaultError::InvalidKdfParams("rejected by Argon2"))?;
    Ok(key)
}

fn key_check_aad(prefix: &[u8]) -> Vec<u8> {
    let mut aad = Vec::with_capacity(KEY_CHECK_CONTEXT.len() + prefix.len());
    aad.extend_from_slice(KEY_CHECK_CONTEXT);
    aad.extend_from_slice(prefix);
    aad
}

fn key_check_tag(
    cipher: &XChaCha20Poly1305,
    nonce: &[u8; NONCE_LEN],
    prefix: &[u8],
) -> Result<[u8; TAG_LEN], VaultError> {
    let mut buf = Vec::with_capacity(TAG_LEN);
    cipher
        .encrypt_in_place(&XNonce::from(*nonce), &key_check_aad(prefix), &mut buf)
        .map_err(|_| VaultError::Crypto)?;
    buf.as_slice().try_into().map_err(|_| VaultError::Crypto)
}

/// Constant-time tag verification (done by the AEAD).
fn verify_key_check(
    cipher: &XChaCha20Poly1305,
    nonce: &[u8; NONCE_LEN],
    tag: &[u8; TAG_LEN],
    prefix: &[u8],
) -> bool {
    let mut buf = tag.to_vec();
    cipher
        .decrypt_in_place(&XNonce::from(*nonce), &key_check_aad(prefix), &mut buf)
        .is_ok()
}

/// [`SecretStore`] view of a [`Vault`] for one project (secrets keyed by the project UUID,
/// ADR-0017 §4).
///
/// Mapping to [`SecretError`]: a locked vault (or one that does not exist yet, for writes) is
/// [`SecretError::Locked`]`(Vault)`: the core asks for the master password, calls
/// [`Vault::unlock`] or [`Vault::create`], and retries. Reading from a vault that does not exist
/// is [`SecretError::NotFound`]; deleting from it returns `Ok(false)`.
#[derive(Debug, Clone)]
pub struct VaultStore {
    vault: Arc<Vault>,
    project: String,
}

impl VaultStore {
    /// The secrets of one project, identified by its stable UUID from `project.json`.
    ///
    /// # Errors
    /// [`InvalidProjectId`] unless `project_id` is a hyphenated UUID.
    pub fn new(vault: Arc<Vault>, project_id: &str) -> Result<Self, InvalidProjectId> {
        Ok(Self {
            vault,
            project: normalize_project_id(project_id)?,
        })
    }

    /// The shared vault (to lock, unlock or check its state).
    #[must_use]
    pub fn vault(&self) -> &Arc<Vault> {
        &self.vault
    }

    /// Identifiers of this project's secrets.
    ///
    /// # Errors
    /// [`VaultError::Locked`], [`VaultError::NotCreated`].
    pub fn ids(&self) -> Result<Vec<SecretId>, VaultError> {
        self.vault.ids(&self.project)
    }
}

fn to_secret_error(e: VaultError) -> SecretError {
    match e {
        VaultError::Locked
        | VaultError::NotCreated
        | VaultError::WrongPassword
        | VaultError::TooManyAttempts { .. } => SecretError::Locked(StoreKind::Vault),
        other => SecretError::Backend {
            store: StoreKind::Vault,
            detail: other.to_string(),
        },
    }
}

impl SecretStore for VaultStore {
    fn kind(&self) -> StoreKind {
        StoreKind::Vault
    }

    /// A vault that exists and is locked (a vault not created yet holds nothing: not locked).
    fn is_locked(&self) -> bool {
        matches!(self.vault.status(), Ok(VaultStatus::Locked))
    }

    fn get(&self, id: &SecretId) -> Result<SecretString, SecretError> {
        match self.vault.get(&self.project, id) {
            Ok(Some(secret)) => Ok(secret),
            Ok(None) | Err(VaultError::NotCreated) => Err(SecretError::NotFound(id.clone())),
            Err(e) => Err(to_secret_error(e)),
        }
    }

    fn set(
        &self,
        id: &SecretId,
        meta: &SecretMeta,
        secret: &SecretString,
    ) -> Result<(), SecretError> {
        self.vault
            .set(&self.project, id, meta, secret)
            .map_err(to_secret_error)
    }

    fn delete(&self, id: &SecretId) -> Result<bool, SecretError> {
        match self.vault.delete(&self.project, id) {
            Err(VaultError::NotCreated) => Ok(false),
            other => other.map_err(to_secret_error),
        }
    }
}
