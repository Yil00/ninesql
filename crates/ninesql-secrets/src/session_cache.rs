//! In-memory cache for "ask every time" secrets (ADR-0005 §3, F-CON-04).
//!
//! A connection whose password is not saved has no [`crate::SecretRef`]: the core asks the user at
//! connect time. To avoid asking again for every metadata connection or reconnect of the same
//! session, the core may keep the answer here, **in memory only**, with an explicit lifetime:
//!
//! - [`CacheLifetime::Session`]: until the core removes it ([`SessionSecretCache::remove`],
//!   [`SessionSecretCache::remove_connection`] on disconnect, [`SessionSecretCache::clear`] when the
//!   application locks or exits);
//! - [`CacheLifetime::For`]: additionally expires after a duration.
//!
//! Every value is a [`SecretString`], zeroized when it is removed, expires, is replaced, or when
//! the cache is dropped. Nothing here touches the disk.
//!
//! Expiry is measured on `CLOCK_BOOTTIME`, so time spent suspended counts: a value cached "for
//! 15 minutes" before an overnight suspend is gone at the first access or purge after resume. A
//! duration too large to represent is capped at [`MAX_TIMED_LIFETIME`] (fail closed: the value
//! still expires). The core should also call [`SessionSecretCache::clear`] on logind's
//! `PrepareForSleep(true)` and on the session `Lock` signal.
//!
//! Intended use in the core (one cache per open workspace):
//! 1. On connect, for each slot without a saved reference, look up
//!    `cache.get(&SecretId("<connection uuid>:<slot>"))`.
//! 2. If absent, prompt (UI), then `cache.insert(id, secret, lifetime)` if the user did not ask to
//!    forget it immediately.
//! 3. On disconnect of the last session of the connection: `cache.remove_connection(uuid)`; on
//!    application lock / exit: `cache.clear()`. Call [`SessionSecretCache::purge_expired`] from
//!    the same timer that calls [`crate::Vault::lock_if_idle`].
//! 4. A failed authentication must `remove` the cached value so the next attempt prompts again.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use secrecy::{ExposeSecret, SecretString};

use crate::clock::{self, BootInstant, SharedClock};
use crate::reference::SecretId;

/// Longest effective [`CacheLifetime::For`] (100 years); longer durations are capped to it.
pub const MAX_TIMED_LIFETIME: Duration = Duration::from_secs(100 * 365 * 24 * 60 * 60);

/// How long a cached "ask every time" secret is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheLifetime {
    /// Until removed explicitly (disconnect, lock, exit).
    Session,
    /// Until removed explicitly, or at most this long (capped at [`MAX_TIMED_LIFETIME`]).
    For(Duration),
}

struct Cached {
    secret: SecretString,
    expires_at: Option<BootInstant>,
}

/// In-memory, per-session store of secrets the user typed at connect time. Never persisted.
pub struct SessionSecretCache {
    items: Mutex<HashMap<SecretId, Cached>>,
    clock: SharedClock,
}

impl Default for SessionSecretCache {
    fn default() -> Self {
        Self::with_clock(clock::system())
    }
}

impl fmt::Debug for SessionSecretCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionSecretCache")
            .field("len", &self.items().len())
            .finish_non_exhaustive()
    }
}

impl SessionSecretCache {
    /// An empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub(crate) fn with_clock(clock: SharedClock) -> Self {
        Self {
            items: Mutex::new(HashMap::new()),
            clock,
        }
    }

    fn items(&self) -> MutexGuard<'_, HashMap<SecretId, Cached>> {
        self.items
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Keeps `secret` under `id` (replacing, and zeroizing, any previous value).
    pub fn insert(&self, id: SecretId, secret: SecretString, lifetime: CacheLifetime) {
        let expires_at = match lifetime {
            CacheLifetime::Session => None,
            CacheLifetime::For(d) => {
                Some(self.clock.now().saturating_add(d.min(MAX_TIMED_LIFETIME)))
            }
        };
        tracing::debug!(id = %id, "caching an ask-every-time secret in memory");
        self.items().insert(id, Cached { secret, expires_at });
    }

    /// A copy of the cached secret, unless absent or expired (an expired value is dropped).
    #[must_use]
    pub fn get(&self, id: &SecretId) -> Option<SecretString> {
        self.get_at(id, self.clock.now())
    }

    fn get_at(&self, id: &SecretId, now: BootInstant) -> Option<SecretString> {
        let mut items = self.items();
        let expired = items
            .get(id)
            .is_some_and(|c| c.expires_at.is_some_and(|at| at <= now));
        if expired {
            items.remove(id);
            return None;
        }
        items
            .get(id)
            .map(|c| SecretString::from(c.secret.expose_secret().to_owned()))
    }

    /// Forgets one secret. Returns whether it was cached.
    pub fn remove(&self, id: &SecretId) -> bool {
        self.items().remove(id).is_some()
    }

    /// Forgets every secret of a connection (ids `<connection id>:<slot>`). Returns how many.
    pub fn remove_connection(&self, connection_id: &str) -> usize {
        let mut items = self.items();
        let before = items.len();
        items.retain(|id, _| {
            id.as_str()
                .split_once(':')
                .is_none_or(|(conn, _)| conn != connection_id)
        });
        before - items.len()
    }

    /// Drops expired secrets. Returns how many.
    pub fn purge_expired(&self) -> usize {
        self.purge_expired_at(self.clock.now())
    }

    fn purge_expired_at(&self, now: BootInstant) -> usize {
        let mut items = self.items();
        let before = items.len();
        items.retain(|_, c| c.expires_at.is_none_or(|at| at > now));
        before - items.len()
    }

    /// Forgets everything.
    pub fn clear(&self) {
        self.items().clear();
    }

    /// Number of cached secrets.
    #[must_use]
    pub fn len(&self) -> usize {
        self.items().len()
    }

    /// Whether nothing is cached.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).
mod tests {
    use super::*;

    fn id(s: &str) -> SecretId {
        SecretId::new(s).unwrap()
    }

    fn secret(s: &str) -> SecretString {
        SecretString::from(s.to_owned())
    }

    #[test]
    fn session_lifetime_and_removal() {
        let cache = SessionSecretCache::new();
        cache.insert(id("c1:auth"), secret("pw1"), CacheLifetime::Session);
        cache.insert(id("c1:ssh"), secret("pw2"), CacheLifetime::Session);
        cache.insert(id("c2:auth"), secret("pw3"), CacheLifetime::Session);
        assert_eq!(cache.get(&id("c1:auth")).unwrap().expose_secret(), "pw1");
        assert!(!format!("{cache:?}").contains("pw1"));
        assert_eq!(cache.remove_connection("c1"), 2);
        assert!(cache.get(&id("c1:ssh")).is_none());
        assert!(cache.remove(&id("c2:auth")));
        assert!(!cache.remove(&id("c2:auth")));
        cache.insert(id("c3:auth"), secret("x"), CacheLifetime::Session);
        cache.clear();
        assert!(cache.is_empty());
    }

    #[test]
    fn replacing_keeps_the_new_value() {
        let cache = SessionSecretCache::new();
        cache.insert(id("c:auth"), secret("old"), CacheLifetime::Session);
        cache.insert(id("c:auth"), secret("new"), CacheLifetime::Session);
        assert_eq!(cache.get(&id("c:auth")).unwrap().expose_secret(), "new");
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn timed_lifetime_expires() {
        let clock = clock::ManualClock::new();
        let cache = SessionSecretCache::with_clock(clock.clone());
        cache.insert(
            id("c:auth"),
            secret("pw"),
            CacheLifetime::For(Duration::from_secs(60)),
        );
        cache.insert(id("c:ssh"), secret("pw"), CacheLifetime::Session);
        assert!(cache.get(&id("c:auth")).is_some());
        clock.advance(Duration::from_secs(61));
        assert!(cache.get(&id("c:auth")).is_none());
        assert_eq!(cache.len(), 1);
        cache.insert(
            id("d:auth"),
            secret("pw"),
            CacheLifetime::For(Duration::ZERO),
        );
        assert_eq!(cache.purge_expired(), 1);
        assert_eq!(cache.purge_expired(), 0);
        assert_eq!(cache.len(), 1);
    }

    /// L6: a duration that overflows is capped, so the value still expires (fail closed).
    #[test]
    fn huge_lifetime_is_capped_not_infinite() {
        let clock = clock::ManualClock::new();
        let cache = SessionSecretCache::with_clock(clock.clone());
        cache.insert(
            id("c:auth"),
            secret("pw"),
            CacheLifetime::For(Duration::MAX),
        );
        clock.advance(MAX_TIMED_LIFETIME - Duration::from_secs(1));
        assert!(cache.get(&id("c:auth")).is_some());
        clock.advance(Duration::from_secs(1));
        assert!(cache.get(&id("c:auth")).is_none());
    }
}
