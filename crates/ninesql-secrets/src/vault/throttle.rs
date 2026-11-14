//! Back-off between failed unlock attempts.
//!
//! This slows down someone guessing master passwords **through NineSQL** (at an unattended,
//! logged-in desktop). It does not protect a copied `vault.bin` against offline guessing; the
//! Argon2id cost does that.

use std::time::{Duration, Instant};

/// Back-off policy for failed unlock (and change-password) attempts.
///
/// After `free_attempts` consecutive failures, each new failure doubles the wait before the next
/// attempt, starting at `base_delay` and capped at `max_delay`. A successful unlock resets it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnlockBackoff {
    /// Failures allowed before any wait.
    pub free_attempts: u32,
    /// Wait after the first failure beyond `free_attempts`.
    pub base_delay: Duration,
    /// Longest wait.
    pub max_delay: Duration,
}

impl Default for UnlockBackoff {
    /// 3 free attempts, then 1 s, 2 s, 4 s… up to 60 s.
    fn default() -> Self {
        Self {
            free_attempts: 3,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(60),
        }
    }
}

/// Failure counter driving an [`UnlockBackoff`]. Time is passed in, so it is testable.
#[derive(Debug, Clone)]
pub(crate) struct UnlockThrottle {
    policy: UnlockBackoff,
    failures: u32,
    next_allowed: Option<Instant>,
}

impl UnlockThrottle {
    pub(crate) fn new(policy: UnlockBackoff) -> Self {
        Self {
            policy,
            failures: 0,
            next_allowed: None,
        }
    }

    pub(crate) fn set_policy(&mut self, policy: UnlockBackoff) {
        self.policy = policy;
    }

    /// `Err(wait)` while an attempt is not allowed yet.
    pub(crate) fn check(&self, now: Instant) -> Result<(), Duration> {
        match self.next_allowed {
            Some(at) if at > now => Err(at - now),
            _ => Ok(()),
        }
    }

    pub(crate) fn record_failure(&mut self, now: Instant) {
        self.failures = self.failures.saturating_add(1);
        if self.failures > self.policy.free_attempts {
            let exponent = (self.failures - self.policy.free_attempts - 1).min(31);
            let delay = self
                .policy
                .base_delay
                .saturating_mul(1u32 << exponent)
                .min(self.policy.max_delay);
            self.next_allowed = Some(now + delay);
        }
    }

    pub(crate) fn record_success(&mut self) {
        self.failures = 0;
        self.next_allowed = None;
    }

    #[cfg(test)]
    pub(crate) fn failures(&self) -> u32 {
        self.failures
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).
mod tests {
    use super::*;

    #[test]
    fn free_attempts_then_doubling_capped() {
        let mut t = UnlockThrottle::new(UnlockBackoff::default());
        let t0 = Instant::now();
        for _ in 0..3 {
            assert!(t.check(t0).is_ok());
            t.record_failure(t0);
        }
        // Third failure was free: next attempt allowed at once.
        assert!(t.check(t0).is_ok());
        t.record_failure(t0); // 4th failure: wait 1 s
        assert_eq!(t.check(t0), Err(Duration::from_secs(1)));
        assert!(t.check(t0 + Duration::from_secs(1)).is_ok());
        let t1 = t0 + Duration::from_secs(1);
        t.record_failure(t1); // 5th: 2 s
        assert_eq!(t.check(t1), Err(Duration::from_secs(2)));
        for _ in 0..40 {
            t.record_failure(t1);
        }
        assert_eq!(t.check(t1), Err(Duration::from_secs(60)));
        t.record_success();
        assert!(t.check(t1).is_ok());
        assert_eq!(t.failures(), 0);
    }
}
