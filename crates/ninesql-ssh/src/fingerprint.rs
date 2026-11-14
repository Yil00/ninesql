//! Host key fingerprints in the OpenSSH `SHA256:<base64>` format (ADR-0011 §6).
//!
//! This is what the UI shows when it asks the user to confirm an unknown host key, and what
//! `ssh-keygen -lf` prints, so the user can compare it with the server's own output.

use std::fmt;

use data_encoding::BASE64_NOPAD;
use sha2::{Digest, Sha256};

/// SHA-256 fingerprint of an SSH public key blob (the wire encoding, as in `known_hosts`).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Fingerprint([u8; 32]);

impl Fingerprint {
    /// Fingerprint of a public key blob (SSH wire encoding).
    #[must_use]
    pub fn of_blob(blob: &[u8]) -> Self {
        Self(Sha256::digest(blob).into())
    }

    /// Raw digest.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// `SHA256:` followed by unpadded standard base64, exactly like OpenSSH.
impl fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SHA256:{}", BASE64_NOPAD.encode(&self.0))
    }
}

impl fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).
mod tests {
    use super::*;
    use data_encoding::BASE64;

    /// A throwaway Ed25519 public key (no matching private key is kept anywhere).
    const ED25519_LINE_KEY: &str =
        "AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl";

    #[test]
    fn format_is_openssh_sha256() {
        let blob = BASE64.decode(ED25519_LINE_KEY.as_bytes()).unwrap();
        let fp = Fingerprint::of_blob(&blob);
        let s = fp.to_string();
        assert!(s.starts_with("SHA256:"));
        // 32 bytes -> 43 base64 characters without padding.
        assert_eq!(s.len(), "SHA256:".len() + 43);
        assert!(!s.ends_with('='));
        assert_eq!(format!("{fp:?}"), s);
    }

    #[test]
    fn known_vector() {
        // SHA-256 of the empty input, base64 without padding.
        let fp = Fingerprint::of_blob(b"");
        assert_eq!(
            fp.to_string(),
            "SHA256:47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU"
        );
    }

    #[test]
    fn matches_ssh_key_crate() {
        use russh::keys::{HashAlg, PublicKey};
        let key = PublicKey::from_openssh(&format!("ssh-ed25519 {ED25519_LINE_KEY}")).unwrap();
        let ours = Fingerprint::of_blob(&key.to_bytes().unwrap());
        assert_eq!(
            ours.to_string(),
            key.fingerprint(HashAlg::Sha256).to_string()
        );
    }
}
