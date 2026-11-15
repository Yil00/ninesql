//! Locking secrets when the desktop sleeps or locks (ADR-0005; ROADMAP Phase 1 follow-up
//! "P1-D (core)": lock the vault and clear the "ask every time" cache on logind
//! `PrepareForSleep(true)` and on the session `Lock` signal).
//!
//! The pieces:
//! - [`LockHook`]: something that drops secrets from memory when told to. Implemented by
//!   [`Vault`] (locks it: key and decrypted secrets zeroized) and [`SessionSecretCache`]
//!   (cleared: every cached value zeroized).
//! - [`LockHooks`]: the set the application registers once (every open vault, every cache) and
//!   triggers as a whole.
//! - With the `desktop-lock` feature (on by default, and implied by `secret-service`),
//!   `DesktopLockWatcher` listens to logind on the
//!   system bus and calls a hook on `PrepareForSleep(true)` (holding a sleep delay inhibitor
//!   until the hook ran) and when the current session locks (`Lock` signal or `LockedHint`
//!   turning `true`). Without it, the application calls [`LockHooks::lock_all`] from whatever
//!   source it has (a menu action, its own screen-lock detection).
//!
//! Idle timers keep running across suspend (`CLOCK_BOOTTIME`), so this is a second line: the
//! secrets leave memory *before* the machine sleeps or the screen locks, not at the first access
//! after resume.

use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};

use crate::session_cache::SessionSecretCache;
use crate::vault::Vault;

/// Why secrets are being locked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum LockTrigger {
    /// The system is about to suspend or hibernate (logind `PrepareForSleep(true)`).
    Sleep,
    /// The desktop session is being locked (logind session `Lock`).
    SessionLock,
    /// The application asked for it (a "lock now" action, exit).
    Requested,
}

impl LockTrigger {
    /// A short, stable name, safe to log.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sleep => "sleep",
            Self::SessionLock => "session-lock",
            Self::Requested => "requested",
        }
    }
}

impl fmt::Display for LockTrigger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Something that drops secrets from memory when the desktop sleeps or locks. Called from the
/// watcher's thread: it must not block for long and must not panic.
pub trait LockHook: Send + Sync {
    /// Drops (zeroizes) the secrets held in memory.
    fn lock_now(&self, trigger: LockTrigger);
}

impl LockHook for Vault {
    fn lock_now(&self, trigger: LockTrigger) {
        if self.is_unlocked() {
            tracing::info!(trigger = %trigger, "locking the vault");
        }
        self.lock();
    }
}

impl LockHook for SessionSecretCache {
    fn lock_now(&self, trigger: LockTrigger) {
        let cached = self.len();
        if cached > 0 {
            tracing::info!(trigger = %trigger, cached, "clearing the ask-every-time cache");
        }
        self.clear();
    }
}

/// Every [`LockHook`] of the application, triggered together. Cheap to share (`Arc`).
#[derive(Default)]
pub struct LockHooks {
    hooks: Mutex<Vec<Arc<dyn LockHook>>>,
}

impl fmt::Debug for LockHooks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LockHooks")
            .field("hooks", &self.len())
            .finish()
    }
}

impl LockHooks {
    /// No hook yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a hook (a vault, a cache, or any other holder of secrets).
    pub fn add(&self, hook: Arc<dyn LockHook>) {
        self.hooks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(hook);
    }

    /// Number of hooks.
    #[must_use]
    pub fn len(&self) -> usize {
        self.hooks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// Whether no hook is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Calls every hook. The list is copied first, so a hook may register another without
    /// deadlocking.
    pub fn lock_all(&self, trigger: LockTrigger) {
        let hooks = self
            .hooks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        tracing::debug!(trigger = %trigger, hooks = hooks.len(), "locking secrets");
        for hook in hooks {
            hook.lock_now(trigger);
        }
    }
}

impl LockHook for LockHooks {
    fn lock_now(&self, trigger: LockTrigger) {
        self.lock_all(trigger);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use secrecy::SecretString;

    use super::*;
    use crate::reference::SecretId;
    use crate::session_cache::CacheLifetime;
    use crate::vault::KdfParams;

    fn s(v: &str) -> SecretString {
        SecretString::from(v.to_owned())
    }

    #[test]
    fn lock_all_locks_the_vault_and_clears_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let vault = Arc::new(
            Vault::open_at(dir.path().join("vault.bin"), &dir.path().join("vault.lock")).unwrap(),
        );
        vault
            .create(&s("master-pw"), KdfParams::insecure_for_tests())
            .unwrap();
        assert!(vault.is_unlocked());
        let cache = Arc::new(SessionSecretCache::new());
        cache.insert(
            SecretId::new("c:auth").unwrap(),
            s("typed-pw"),
            CacheLifetime::Session,
        );

        let hooks = LockHooks::new();
        assert!(hooks.is_empty());
        hooks.add(vault.clone());
        hooks.add(cache.clone());
        assert_eq!(hooks.len(), 2);

        hooks.lock_all(LockTrigger::Sleep);
        assert!(!vault.is_unlocked());
        assert!(cache.is_empty());

        // Idempotent, and the vault opens again with its password.
        hooks.lock_all(LockTrigger::SessionLock);
        vault.unlock(&s("master-pw")).unwrap();
        assert!(vault.is_unlocked());
        hooks.lock_now(LockTrigger::Requested);
        assert!(!vault.is_unlocked());
    }

    struct Counter(AtomicUsize);

    impl LockHook for Counter {
        fn lock_now(&self, _: LockTrigger) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// A hook that registers another while it runs.
    struct Reentrant(Arc<LockHooks>, Arc<Counter>);

    impl LockHook for Reentrant {
        fn lock_now(&self, _: LockTrigger) {
            self.0.add(self.1.clone());
        }
    }

    #[test]
    fn every_hook_runs_and_a_hook_may_register_another() {
        let hooks = Arc::new(LockHooks::new());
        let counter = Arc::new(Counter(AtomicUsize::new(0)));
        hooks.add(counter.clone());
        hooks.add(Arc::new(Reentrant(hooks.clone(), counter.clone())));
        hooks.lock_all(LockTrigger::Requested);
        assert_eq!(counter.0.load(Ordering::SeqCst), 1);
        hooks.lock_all(LockTrigger::Requested);
        assert_eq!(counter.0.load(Ordering::SeqCst), 3);
        assert_eq!(LockTrigger::Sleep.to_string(), "sleep");
        assert_eq!(LockTrigger::SessionLock.as_str(), "session-lock");
    }
}
