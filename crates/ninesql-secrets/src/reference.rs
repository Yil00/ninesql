//! Secret references: what workspace files store instead of a secret (ADR-0005 §3).

use std::fmt;

use serde::{Deserialize, Serialize};

/// Which secret store holds a secret.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StoreKind {
    /// The freedesktop Secret Service (GNOME Keyring, KWallet, KeePassXC).
    Keyring,
    /// The encrypted local vault (`vault.bin`), used when no Secret Service answers.
    Vault,
}

impl fmt::Display for StoreKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Keyring => "keyring",
            Self::Vault => "vault",
        })
    }
}

/// What a secret is used for. Stored as the `kind` attribute of a Secret Service item (ADR-0005 §1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretKind {
    /// Database password.
    Password,
    /// SSH password of a tunnel hop.
    SshPassword,
    /// Passphrase of an SSH private key.
    SshKeyPassphrase,
    /// API or access token.
    Token,
    /// Passphrase of a TLS client certificate key.
    ClientKeyPassphrase,
    /// Password of an HTTP / SOCKS proxy (ADR-0011 §8). Its slot is `proxy` (secret id
    /// `<connection uuid>:proxy`); the connection model has no proxy field yet, so no slot
    /// currently produces this kind.
    ProxyPassword,
}

impl SecretKind {
    /// Stable attribute value, identical to the serde name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Password => "password",
            Self::SshPassword => "ssh_password",
            Self::SshKeyPassphrase => "ssh_key_passphrase",
            Self::Token => "token",
            Self::ClientKeyPassphrase => "client_key_passphrase",
            Self::ProxyPassword => "proxy_password",
        }
    }
}

impl fmt::Display for SecretKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Maximum length of a [`SecretId`].
pub const MAX_SECRET_ID_LEN: usize = 128;

/// Error returned when a string is not a valid [`SecretId`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "invalid secret identifier: 1 to {MAX_SECRET_ID_LEN} ASCII letters, digits, `-`, `_`, `.` or `:` expected"
)]
pub struct InvalidSecretId;

/// Opaque identifier of a secret inside one store.
///
/// For connection secrets the format is `<connection uuid>:<slot>` (ADR-0012 §4), with these
/// slots: `auth` ([`SecretKind::Password`] or [`SecretKind::Token`]), `ssh` and `ssh-jump-N`
/// ([`SecretKind::SshPassword`] or [`SecretKind::SshKeyPassphrase`]), `tls-client-key`
/// ([`SecretKind::ClientKeyPassphrase`]) and `proxy` ([`SecretKind::ProxyPassword`], reserved
/// for the proxy hop of ADR-0011 §8 until the connection model gets a proxy field).
///
/// Restricted to a short ASCII alphabet so it can be used as a D-Bus attribute value and a vault
/// key, and so it cannot plausibly hold a pasted password with spaces or symbols. The identifier
/// is not secret: it appears in workspace files and logs.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SecretId(String);

impl SecretId {
    /// Validates and wraps an identifier.
    ///
    /// # Errors
    /// [`InvalidSecretId`] if the string is empty, too long or uses other characters.
    pub fn new(id: impl Into<String>) -> Result<Self, InvalidSecretId> {
        let id = id.into();
        let valid = !id.is_empty()
            && id.len() <= MAX_SECRET_ID_LEN
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'));
        if valid {
            Ok(Self(id))
        } else {
            Err(InvalidSecretId)
        }
    }

    /// The identifier as a string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SecretId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for SecretId {
    type Error = InvalidSecretId;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<SecretId> for String {
    fn from(value: SecretId) -> Self {
        value.0
    }
}

/// Reference to a secret, the only form in which a secret appears in workspace files.
///
/// Serialized as `{ "store": "keyring" | "vault", "id": "…" }` (ADR-0005 §3).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretRef {
    /// Store holding the secret.
    pub store: StoreKind,
    /// Identifier inside that store.
    pub id: SecretId,
}

impl SecretRef {
    /// Builds a reference.
    #[must_use]
    pub const fn new(store: StoreKind, id: SecretId) -> Self {
        Self { store, id }
    }
}

impl fmt::Display for SecretRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.store, self.id)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).
mod tests {
    use super::*;

    #[test]
    fn secret_id_validation() {
        assert!(SecretId::new("3f2a-01:password").is_ok());
        assert!(SecretId::new("").is_err());
        assert!(SecretId::new("has space").is_err());
        assert!(SecretId::new("p@ss!").is_err());
        assert!(SecretId::new("a".repeat(MAX_SECRET_ID_LEN)).is_ok());
        assert!(SecretId::new("a".repeat(MAX_SECRET_ID_LEN + 1)).is_err());
    }

    #[test]
    fn secret_ref_json_shape() {
        let r = SecretRef::new(StoreKind::Keyring, SecretId::new("abc:password").unwrap());
        let json = serde_json::to_string(&r).unwrap();
        assert_eq!(json, r#"{"store":"keyring","id":"abc:password"}"#);
        let back: SecretRef = serde_json::from_str(&json).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn secret_ref_rejects_bad_input() {
        assert!(serde_json::from_str::<SecretRef>(r#"{"store":"plain","id":"x"}"#).is_err());
        assert!(serde_json::from_str::<SecretRef>(r#"{"store":"vault","id":"a b"}"#).is_err());
        // A reference never carries a value field.
        assert!(
            serde_json::from_str::<SecretRef>(r#"{"store":"vault","id":"x","value":"hunter2"}"#)
                .is_err()
        );
    }

    #[test]
    fn kind_names_match_serde() {
        for kind in [
            SecretKind::Password,
            SecretKind::SshPassword,
            SecretKind::SshKeyPassphrase,
            SecretKind::Token,
            SecretKind::ClientKeyPassphrase,
            SecretKind::ProxyPassword,
        ] {
            assert_eq!(
                serde_json::to_string(&kind).unwrap(),
                format!("\"{}\"", kind.as_str())
            );
        }
    }
}
