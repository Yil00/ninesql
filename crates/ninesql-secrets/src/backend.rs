//! Backend selection: Secret Service when one answers, otherwise the encrypted vault, never
//! plaintext (ADR-0005 §1–§3, invariant I1).
//!
//! Startup sequence for the core (on a blocking thread):
//! 1. `detect_secret_service()`. If [`SecretServiceAvailability::Available`], register a
//!    `SecretServiceStore` for the project (feature `secret-service`).
//! 2. Always register a [`crate::VaultStore`] on the shared [`crate::Vault`] (opened locked):
//!    references already saved in the vault must keep resolving, whatever the keyring does.
//! 3. New secrets go to [`crate::SecretStores::store_for_new_secrets`]: the keyring if registered,
//!    else the vault (the core asks for the master password, creating the vault on first use).
//!    With neither, saving is impossible and the core offers "ask every time" only.
//!
//! There is no automatic fallback at runtime: if a keyring call fails (`Unavailable`, `Locked`),
//! the core reports it and lets the user move the secret to the vault explicitly; a secret is
//! never written anywhere the user did not choose, and never in clear.

/// Whether a Secret Service provider answers on the D-Bus session bus.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SecretServiceAvailability {
    /// A provider answered and has a default collection.
    Available,
    /// No session bus, no provider, or no default collection.
    Unavailable {
        /// Transport-level reason, without any secret.
        detail: String,
    },
    /// This build has no Secret Service support (feature `secret-service` off).
    NotBuilt,
}

impl SecretServiceAvailability {
    /// Whether the keyring backend can be used.
    #[must_use]
    pub fn is_available(&self) -> bool {
        matches!(self, Self::Available)
    }
}

/// Probes the session bus for a Secret Service provider (blocking for at most 5 s; opens an
/// encrypted session, reads the `default` collection alias, transfers no secret, triggers no
/// prompt). A provider that does not answer within 5 s, or a failure inside the client, is
/// reported as `Unavailable`.
#[must_use]
pub fn detect_secret_service() -> SecretServiceAvailability {
    #[cfg(feature = "secret-service")]
    {
        crate::secret_service::probe()
    }
    #[cfg(not(feature = "secret-service"))]
    {
        SecretServiceAvailability::NotBuilt
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(feature = "secret-service"))]
    #[test]
    fn not_built_without_the_feature() {
        assert_eq!(detect_secret_service(), SecretServiceAvailability::NotBuilt);
        assert!(!detect_secret_service().is_available());
    }

    /// Without any Secret Service provider (or without a session bus), detection reports
    /// `Unavailable` and does not hang or panic. Opt-in: run with
    /// `NINESQL_TEST_EXPECT_NO_SECRET_SERVICE=1`, under `dbus-run-session` or with no bus at all.
    #[cfg(feature = "secret-service")]
    #[test]
    fn unavailable_without_provider() {
        if std::env::var_os("NINESQL_TEST_EXPECT_NO_SECRET_SERVICE").is_none() {
            return;
        }
        let got = detect_secret_service();
        assert!(
            matches!(got, SecretServiceAvailability::Unavailable { .. }),
            "{got:?}"
        );
    }
}
