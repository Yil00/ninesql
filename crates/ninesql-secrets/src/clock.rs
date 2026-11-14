//! Clock for idle timeouts, cache expiry and the unlock back-off.
//!
//! [`std::time::Instant`] reads `CLOCK_MONOTONIC`, which **stops while the machine is
//! suspended**: a vault with a 15-minute idle timeout left unlocked before an overnight suspend
//! would still be unlocked in the morning. Deadlines here use `CLOCK_BOOTTIME` instead (time since
//! boot, suspend included), read through `rustix` without `unsafe`.
//!
//! Suspend still pauses the timer that calls [`crate::Vault::lock_if_idle`] and
//! [`crate::SessionSecretCache::purge_expired`]: the deadline is applied at the first check after
//! resume (every vault and cache access checks it too). The application should additionally lock
//! the vault and clear the cache on logind's `PrepareForSleep(true)` and on the session `Lock`
//! signal, so nothing is unlocked when the machine wakes up: register both in a
//! [`crate::LockHooks`] and, with the `secret-service` feature, start a
//! `DesktopLockWatcher` on it ([`crate::lock_events`]).

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

/// A point in time on `CLOCK_BOOTTIME`, as time since boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct BootInstant(Duration);

impl BootInstant {
    /// Time elapsed since `earlier`, zero if `earlier` is later.
    pub(crate) fn saturating_duration_since(self, earlier: Self) -> Duration {
        self.0.saturating_sub(earlier.0)
    }

    /// `self + d`, saturating (never wraps, so a deadline never moves into the past).
    pub(crate) fn saturating_add(self, d: Duration) -> Self {
        Self(self.0.saturating_add(d))
    }

    #[cfg(test)]
    pub(crate) const fn from_since_boot(d: Duration) -> Self {
        Self(d)
    }
}

/// Source of [`BootInstant`]s; replaced by a manual clock in tests.
pub(crate) trait Clock: Send + Sync + fmt::Debug {
    fn now(&self) -> BootInstant;
}

/// The system's `CLOCK_BOOTTIME`.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct BootClock;

impl Clock for BootClock {
    fn now(&self) -> BootInstant {
        let ts = rustix::time::clock_gettime(rustix::time::ClockId::Boottime);
        // The kernel returns tv_sec >= 0 and 0 <= tv_nsec < 1e9 for this clock.
        let secs = u64::try_from(ts.tv_sec).unwrap_or(0);
        let nanos = u32::try_from(ts.tv_nsec).unwrap_or(0).min(999_999_999);
        BootInstant(Duration::new(secs, nanos))
    }
}

pub(crate) type SharedClock = Arc<dyn Clock>;

/// The system clock, shared.
pub(crate) fn system() -> SharedClock {
    Arc::new(BootClock)
}

/// A clock moved by hand, for tests.
#[cfg(test)]
#[derive(Debug)]
pub(crate) struct ManualClock(std::sync::Mutex<Duration>);

#[cfg(test)]
impl ManualClock {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self(std::sync::Mutex::new(Duration::from_secs(1_000))))
    }

    pub(crate) fn advance(&self, d: Duration) {
        let mut t = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *t = t.saturating_add(d);
    }
}

#[cfg(test)]
impl Clock for ManualClock {
    fn now(&self) -> BootInstant {
        BootInstant(
            *self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boot_clock_moves_forward_like_the_monotonic_clock() {
        let a = BootClock.now();
        let start = std::time::Instant::now();
        std::thread::sleep(Duration::from_millis(20));
        let b = BootClock.now();
        let elapsed = b.saturating_duration_since(a);
        assert!(elapsed >= Duration::from_millis(20), "{elapsed:?}");
        // Without a suspend in between, it does not run much faster than CLOCK_MONOTONIC.
        assert!(
            elapsed <= start.elapsed() + Duration::from_millis(50),
            "{elapsed:?}"
        );
        assert!(a.0 > Duration::ZERO);
    }

    #[test]
    fn saturating_arithmetic() {
        let t = BootInstant::from_since_boot(Duration::from_secs(5));
        assert_eq!(t.saturating_add(Duration::MAX).0, Duration::MAX);
        assert_eq!(
            t.saturating_duration_since(t.saturating_add(Duration::from_secs(1))),
            Duration::ZERO
        );
    }
}
