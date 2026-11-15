//! Host key policy per hop and the decision taken on a lookup result (ADR-0011 §6, I5).

use ninesql_driver_api::HostKeyVerification;

use crate::known_hosts::HostKeyStatus;

/// What to do with a server's host key. Defaults to [`HostKeyPolicy::Strict`].
///
/// Set per hop (one policy per hop, which answers review item L1 of P0-B). Anything but
/// `Strict` is a downgrade the UI must flag visibly ([`crate::SshSecurityWarning`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum HostKeyPolicy {
    /// Known key: accept. Unknown key: the user must confirm the fingerprint. Changed or
    /// revoked key: hard error.
    #[default]
    Strict,
    /// Like `Strict`, but an unknown key is accepted without asking and recorded in NineSQL's
    /// `known_hosts` (OpenSSH `StrictHostKeyChecking=accept-new`). Changed keys still block, and
    /// a host already known under another (unsupported) key type still needs confirmation.
    /// Explicit per-connection choice, reported as a security warning.
    AcceptNew,
    /// The host key is not checked (except `@revoked` keys, still refused). Explicit
    /// per-connection choice, reported as a security warning.
    Disabled,
}

impl HostKeyPolicy {
    /// `true` for anything weaker than [`HostKeyPolicy::Strict`].
    #[must_use]
    pub fn is_downgrade(self) -> bool {
        self != Self::Strict
    }

    /// Mapping to the driver API enum, one to one (ADR-0016 §2.5): `AcceptNew` is reported as
    /// [`HostKeyVerification::AcceptNew`], with its own security warning
    /// ([`ninesql_driver_api::SecurityWarning::SshHostKeyAcceptNew`]).
    #[must_use]
    pub fn to_verification(self) -> HostKeyVerification {
        match self {
            Self::Strict => HostKeyVerification::Strict,
            Self::AcceptNew => HostKeyVerification::AcceptNew,
            Self::Disabled => HostKeyVerification::Disabled,
        }
    }

    /// The weakest of two policies (`Disabled` < `AcceptNew` < `Strict`).
    #[must_use]
    pub fn weakest(self, other: Self) -> Self {
        fn rank(p: HostKeyPolicy) -> u8 {
            match p {
                HostKeyPolicy::Disabled => 0,
                HostKeyPolicy::AcceptNew => 1,
                HostKeyPolicy::Strict => 2,
            }
        }
        if rank(other) < rank(self) {
            other
        } else {
            self
        }
    }
}

/// Decision for one presented key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum HostKeyDecision {
    /// Known key: continue.
    Accept,
    /// Policy says accept without checking (disabled): continue, warning logged.
    AcceptUnverified,
    /// Unknown key under accept-new: continue and record the key in NineSQL's file.
    AcceptAndRecord,
    /// Unknown key under strict: stop and ask the user.
    NeedsConfirmation,
    /// Changed key: hard error.
    RejectChanged,
    /// Revoked key: hard error.
    RejectRevoked,
}

/// Pure policy decision; no I/O.
///
/// [`HostKeyDecision::NeedsConfirmation`] is also returned under [`HostKeyPolicy::AcceptNew`]
/// when the host is on file only under key types NineSQL cannot negotiate
/// (`HostKeyStatus::Unknown` with a non-empty `other_key_types`). In that case the UI must say
/// that **this host is known under another key type** ([`crate::UnknownHostKey::other_key_types`])
/// before offering to trust the new key.
#[must_use]
pub fn decide(policy: HostKeyPolicy, status: &HostKeyStatus) -> HostKeyDecision {
    match (status, policy) {
        (HostKeyStatus::Revoked { .. }, _) => HostKeyDecision::RejectRevoked,
        (_, HostKeyPolicy::Disabled) => HostKeyDecision::AcceptUnverified,
        (HostKeyStatus::Known { .. }, _) => HostKeyDecision::Accept,
        (HostKeyStatus::Changed { .. }, _) => HostKeyDecision::RejectChanged,
        // A host already on file under another (unsupported) key type is not "new": accepting
        // a fresh key silently would let a man-in-the-middle downgrade it. Ask the user.
        (HostKeyStatus::Unknown { other_key_types }, HostKeyPolicy::AcceptNew)
            if !other_key_types.is_empty() =>
        {
            HostKeyDecision::NeedsConfirmation
        }
        (HostKeyStatus::Unknown { .. }, HostKeyPolicy::AcceptNew) => {
            HostKeyDecision::AcceptAndRecord
        }
        (HostKeyStatus::Unknown { .. }, HostKeyPolicy::Strict) => {
            HostKeyDecision::NeedsConfirmation
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).
mod tests {
    use super::*;
    use crate::fingerprint::Fingerprint;
    use crate::known_hosts::KnownHostsSource;

    fn statuses() -> [HostKeyStatus; 4] {
        [
            HostKeyStatus::Known {
                file: KnownHostsSource::NineSql,
                line: 1,
            },
            HostKeyStatus::Changed {
                file: KnownHostsSource::User,
                line: 2,
                known_fingerprint: Fingerprint::of_blob(b"k"),
            },
            HostKeyStatus::Revoked {
                file: KnownHostsSource::User,
                line: 3,
            },
            HostKeyStatus::Unknown {
                other_key_types: vec![],
            },
        ]
    }

    #[test]
    fn default_is_strict() {
        assert_eq!(HostKeyPolicy::default(), HostKeyPolicy::Strict);
        assert!(!HostKeyPolicy::Strict.is_downgrade());
        assert!(HostKeyPolicy::AcceptNew.is_downgrade());
        assert!(HostKeyPolicy::Disabled.is_downgrade());
    }

    #[test]
    fn strict_never_auto_accepts_unknown() {
        use HostKeyDecision as D;
        let got: Vec<_> = statuses()
            .iter()
            .map(|s| decide(HostKeyPolicy::Strict, s))
            .collect();
        assert_eq!(
            got,
            [
                D::Accept,
                D::RejectChanged,
                D::RejectRevoked,
                D::NeedsConfirmation
            ]
        );
    }

    #[test]
    fn accept_new_still_blocks_changed() {
        use HostKeyDecision as D;
        let got: Vec<_> = statuses()
            .iter()
            .map(|s| decide(HostKeyPolicy::AcceptNew, s))
            .collect();
        assert_eq!(
            got,
            [
                D::Accept,
                D::RejectChanged,
                D::RejectRevoked,
                D::AcceptAndRecord
            ]
        );
    }

    #[test]
    fn accept_new_asks_when_host_known_under_other_key_type() {
        let s = HostKeyStatus::Unknown {
            other_key_types: vec!["ssh-rsa".to_owned()],
        };
        assert_eq!(
            decide(HostKeyPolicy::AcceptNew, &s),
            HostKeyDecision::NeedsConfirmation
        );
        assert_eq!(
            decide(HostKeyPolicy::Strict, &s),
            HostKeyDecision::NeedsConfirmation
        );
    }

    #[test]
    fn disabled_accepts_all_but_revoked() {
        use HostKeyDecision as D;
        let got: Vec<_> = statuses()
            .iter()
            .map(|s| decide(HostKeyPolicy::Disabled, s))
            .collect();
        assert_eq!(
            got,
            [
                D::AcceptUnverified,
                D::AcceptUnverified,
                D::RejectRevoked,
                D::AcceptUnverified
            ]
        );
    }

    #[test]
    fn weakest_and_mapping() {
        use HostKeyPolicy as P;
        assert_eq!(P::Strict.weakest(P::AcceptNew), P::AcceptNew);
        assert_eq!(P::AcceptNew.weakest(P::Disabled), P::Disabled);
        assert_eq!(P::Strict.weakest(P::Strict), P::Strict);
        assert_eq!(P::Disabled.weakest(P::Strict), P::Disabled);
        assert_eq!(P::Strict.to_verification(), HostKeyVerification::Strict);
        assert_eq!(
            P::AcceptNew.to_verification(),
            HostKeyVerification::AcceptNew
        );
        assert_eq!(P::Disabled.to_verification(), HostKeyVerification::Disabled);
    }
}
