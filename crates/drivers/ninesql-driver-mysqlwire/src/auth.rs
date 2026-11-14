//! Password scrambles of the authentication plugins the driver supports, computed with `ring`;
//! every buffer derived from the password is zeroized.
//!
//! - `mysql_native_password`: `SHA1(pw) XOR SHA1(nonce || SHA1(SHA1(pw)))`;
//! - `caching_sha2_password` fast path: `SHA256(pw) XOR SHA256(SHA256(SHA256(pw)) || nonce)`.
//!   When the server has no cached verifier it asks for the full exchange, which sends the
//!   password itself: the driver does so only over a protected transport (verified TLS, or a
//!   host-key-verified SSH tunnel to a loopback endpoint) and never asks for the server's RSA
//!   key (`allowPublicKeyRetrieval` is reserved: a man in the middle could send his own).
//!
//! Remnants outside the driver's control: the `Secret` itself (zeroized on drop by
//! `ninesql-driver-api`), and copies inside the TLS stack's buffers for the full exchange.

use ring::digest::{Context, SHA1_FOR_LEGACY_USE_ONLY, SHA256, digest};
use zeroize::Zeroizing;

/// Plugin names.
pub(crate) const NATIVE: &str = "mysql_native_password";
pub(crate) const CACHING_SHA2: &str = "caching_sha2_password";
pub(crate) const CLEAR: &str = "mysql_clear_password";

/// The nonce sent by the server: 20 bytes, without the trailing NUL some servers add.
pub(crate) fn nonce(data: &[u8]) -> &[u8] {
    match data.split_last() {
        Some((0, head)) => head,
        _ => data,
    }
}

/// `mysql_native_password` response (empty for an empty password).
pub(crate) fn native(password: &[u8], nonce: &[u8]) -> Zeroizing<Vec<u8>> {
    if password.is_empty() {
        return Zeroizing::new(Vec::new());
    }
    let stage1 = Zeroizing::new(
        digest(&SHA1_FOR_LEGACY_USE_ONLY, password)
            .as_ref()
            .to_vec(),
    );
    let stage2 = Zeroizing::new(digest(&SHA1_FOR_LEGACY_USE_ONLY, &stage1).as_ref().to_vec());
    let mut ctx = Context::new(&SHA1_FOR_LEGACY_USE_ONLY);
    ctx.update(nonce);
    ctx.update(&stage2);
    let mix = Zeroizing::new(ctx.finish().as_ref().to_vec());
    Zeroizing::new(stage1.iter().zip(mix.iter()).map(|(a, b)| a ^ b).collect())
}

/// `caching_sha2_password` fast-path response (empty for an empty password).
pub(crate) fn caching_sha2(password: &[u8], nonce: &[u8]) -> Zeroizing<Vec<u8>> {
    if password.is_empty() {
        return Zeroizing::new(Vec::new());
    }
    let stage1 = Zeroizing::new(digest(&SHA256, password).as_ref().to_vec());
    let stage2 = Zeroizing::new(digest(&SHA256, &stage1).as_ref().to_vec());
    let mut ctx = Context::new(&SHA256);
    ctx.update(&stage2);
    ctx.update(nonce);
    let mix = Zeroizing::new(ctx.finish().as_ref().to_vec());
    Zeroizing::new(stage1.iter().zip(mix.iter()).map(|(a, b)| a ^ b).collect())
}

/// The password followed by a NUL (full `caching_sha2_password` exchange, `mysql_clear_password`).
pub(crate) fn cleartext(password: &[u8]) -> Zeroizing<Vec<u8>> {
    let mut v = Zeroizing::new(Vec::with_capacity(password.len() + 1));
    v.extend_from_slice(password);
    v.push(0);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn native_matches_the_reference() {
        // Server-side check: SHA1(nonce || stored) XOR response must give SHA1(pw), whose SHA1
        // is the stored hash (`mysql.user.authentication_string` without the `*`).
        let nonce = b"abcdefghijklmnopqrst";
        let r = native(b"secret", nonce);
        let stored = digest(
            &SHA1_FOR_LEGACY_USE_ONLY,
            digest(&SHA1_FOR_LEGACY_USE_ONLY, b"secret").as_ref(),
        );
        assert_eq!(
            hex(stored.as_ref()).to_ascii_uppercase(),
            "14E65567ABDB5135D0CFD9A70B3032C179A49EE7"
        );
        let mut ctx = Context::new(&SHA1_FOR_LEGACY_USE_ONLY);
        ctx.update(nonce);
        ctx.update(stored.as_ref());
        let mix = ctx.finish();
        let stage1: Vec<u8> = r.iter().zip(mix.as_ref()).map(|(a, b)| a ^ b).collect();
        assert_eq!(
            digest(&SHA1_FOR_LEGACY_USE_ONLY, &stage1).as_ref(),
            stored.as_ref()
        );
        assert!(native(b"", nonce).is_empty());
    }

    #[test]
    fn caching_sha2_inverts_with_the_verifier() {
        let nonce = b"0123456789abcdefghij";
        let r = caching_sha2(b"secret", nonce);
        let s1 = digest(&SHA256, b"secret");
        let s2 = digest(&SHA256, s1.as_ref());
        let mut ctx = Context::new(&SHA256);
        ctx.update(s2.as_ref());
        ctx.update(nonce);
        let mix = ctx.finish();
        let back: Vec<u8> = r.iter().zip(mix.as_ref()).map(|(a, b)| a ^ b).collect();
        assert_eq!(back, s1.as_ref());
        assert_eq!(r.len(), 32);
    }

    #[test]
    fn nonce_drops_the_trailing_nul() {
        assert_eq!(nonce(b"abc\0"), b"abc");
        assert_eq!(nonce(b"abc"), b"abc");
        assert_eq!(&cleartext(b"pw")[..], b"pw\0");
    }
}
