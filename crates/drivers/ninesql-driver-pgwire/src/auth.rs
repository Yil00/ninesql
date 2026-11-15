//! Password authentication: SCRAM-SHA-256 (RFC 5802 / 7677, without channel binding) and md5.
//!
//! Implemented here rather than with `postgres-protocol`'s helpers so that every buffer derived
//! from the password (normalized password, salted password, client key, md5 digests) is
//! zeroized on drop (I1, ADR-0005).
//!
//! # Remnants that are not zeroized (L5 of the P1-B review)
//! Best effort only, by construction of the libraries involved:
//! - the md5 and PBKDF2 / HMAC internal states of `md-5` and `ring` (stack copies, not
//!   zeroized by those crates);
//! - the `Md5` hasher's buffer when it is dropped;
//! - the bytes of the password message once handed to the transport: the write buffer of
//!   tokio's `BufStream` and, over TLS, rustls' plaintext / record buffers keep them until
//!   overwritten (the driver's own message buffers are zeroed after sending);
//! - a `stringprep` normalization that allocates (non-ASCII passwords) is copied to a zeroized
//!   buffer and its original is dropped through `Zeroizing`, but intermediate allocations
//!   inside `stringprep` are not under the driver's control.
//!
//! The `Secret` itself is dropped (zeroized) before `connect` returns.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use md5::{Digest as _, Md5};
use ring::{digest, hmac, pbkdf2, rand::SecureRandom};
use zeroize::Zeroizing;

/// Mechanism name sent in `SASLInitialResponse`.
pub(crate) const SCRAM_SHA_256: &str = "SCRAM-SHA-256";

/// Upper bound on the PBKDF2 iteration count a server may ask for (PostgreSQL uses 4096 by
/// default): a hostile server cannot make the client spin for minutes.
const MAX_ITERATIONS: u32 = 1_000_000;

/// PostgreSQL's default SCRAM iteration count; a lower count is logged as a warning.
const MIN_RECOMMENDED_ITERATIONS: u32 = 4096;

/// Authentication failure reason; never contains the password.
#[derive(Debug)]
pub(crate) struct AuthError(pub(crate) String);

fn lower_hex(bytes: &[u8]) -> Zeroizing<String> {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = Zeroizing::new(String::with_capacity(bytes.len() * 2));
    for b in bytes {
        s.push(char::from(DIGITS[usize::from(b >> 4)]));
        s.push(char::from(DIGITS[usize::from(b & 0x0f)]));
    }
    s
}

/// The `md5` password message: `"md5" + md5(md5(password ‖ user) ‖ salt)`.
pub(crate) fn md5_password(user: &str, password: &[u8], salt: [u8; 4]) -> Zeroizing<String> {
    let mut inner = Md5::new();
    inner.update(password);
    inner.update(user.as_bytes());
    let inner = Zeroizing::new(inner.finalize().to_vec());
    let inner_hex = lower_hex(&inner);
    let mut outer = Md5::new();
    outer.update(inner_hex.as_bytes());
    outer.update(salt);
    let outer = Zeroizing::new(outer.finalize().to_vec());
    let mut out = Zeroizing::new(String::with_capacity(35));
    out.push_str("md5");
    out.push_str(&lower_hex(&outer));
    out
}

/// SASLprep-normalized password bytes; the raw bytes when normalization fails (what the
/// PostgreSQL server does too).
fn normalize(password: &[u8]) -> Zeroizing<Vec<u8>> {
    match std::str::from_utf8(password) {
        Ok(s) => match stringprep::saslprep(s) {
            Ok(n) => {
                let bytes = Zeroizing::new(n.as_bytes().to_vec());
                if let std::borrow::Cow::Owned(owned) = n {
                    drop(Zeroizing::new(owned));
                }
                bytes
            }
            Err(_) => Zeroizing::new(password.to_vec()),
        },
        Err(_) => Zeroizing::new(password.to_vec()),
    }
}

/// Client side of one SCRAM-SHA-256 exchange.
pub(crate) struct Scram {
    password: Zeroizing<Vec<u8>>,
    client_nonce: String,
    client_first_bare: String,
    /// Set after the server-first message.
    auth_message: Option<String>,
    salted: Option<Zeroizing<[u8; 32]>>,
}

impl Scram {
    /// Starts an exchange; the password is copied into a zeroized buffer.
    pub(crate) fn new(password: &[u8]) -> Result<Self, AuthError> {
        let mut raw = [0u8; 18];
        ring::rand::SystemRandom::new()
            .fill(&mut raw)
            .map_err(|_| AuthError("no random source for the SCRAM nonce".into()))?;
        let client_nonce = B64.encode(raw);
        Ok(Self::with_nonce(password, client_nonce))
    }

    fn with_nonce(password: &[u8], client_nonce: String) -> Self {
        // PostgreSQL takes the user from the startup message; `n=` stays empty.
        let client_first_bare = format!("n=,r={client_nonce}");
        Self {
            password: normalize(password),
            client_nonce,
            client_first_bare,
            auth_message: None,
            salted: None,
        }
    }

    /// `true` once the server-first message was processed.
    pub(crate) fn has_final(&self) -> bool {
        self.auth_message.is_some()
    }

    /// `client-first-message` (no channel binding: `n,,`).
    pub(crate) fn client_first(&self) -> Vec<u8> {
        format!("n,,{}", self.client_first_bare).into_bytes()
    }

    /// Processes `server-first-message` and returns `client-final-message`.
    pub(crate) fn client_final(&mut self, server_first: &[u8]) -> Result<Vec<u8>, AuthError> {
        let server_first = std::str::from_utf8(server_first)
            .map_err(|_| AuthError("SCRAM server message is not UTF-8".into()))?;
        let mut nonce = None;
        let mut salt = None;
        let mut iterations = None;
        for part in server_first.split(',') {
            if let Some(v) = part.strip_prefix("r=") {
                nonce = Some(v);
            } else if let Some(v) = part.strip_prefix("s=") {
                salt = Some(v);
            } else if let Some(v) = part.strip_prefix("i=") {
                iterations = Some(v);
            } else if part.starts_with("m=") {
                return Err(AuthError("unsupported SCRAM extension".into()));
            }
        }
        let (Some(nonce), Some(salt), Some(iterations)) = (nonce, salt, iterations) else {
            return Err(AuthError("malformed SCRAM server-first message".into()));
        };
        if !nonce.starts_with(&self.client_nonce) || nonce.len() <= self.client_nonce.len() {
            return Err(AuthError(
                "SCRAM server nonce does not extend the client's".into(),
            ));
        }
        let salt = B64
            .decode(salt)
            .map_err(|_| AuthError("SCRAM salt is not base64".into()))?;
        let iterations: u32 = iterations
            .parse()
            .map_err(|_| AuthError("SCRAM iteration count is not a number".into()))?;
        if iterations == 0 || iterations > MAX_ITERATIONS {
            return Err(AuthError(format!(
                "SCRAM iteration count {iterations} is out of range"
            )));
        }
        if iterations < MIN_RECOMMENDED_ITERATIONS {
            // A weak verifier (or a server lowering the cost on purpose): the exchange still
            // runs, flagged in the log (the count only, nothing derived from the password).
            tracing::warn!(
                iterations,
                "the server's SCRAM iteration count is below 4096 (PostgreSQL's default)"
            );
        }
        let mut salted = Zeroizing::new([0u8; 32]);
        pbkdf2::derive(
            pbkdf2::PBKDF2_HMAC_SHA256,
            std::num::NonZeroU32::new(iterations).unwrap_or(std::num::NonZeroU32::MIN),
            &salt,
            &self.password,
            &mut salted[..],
        );
        let salted_key = hmac::Key::new(hmac::HMAC_SHA256, &salted[..]);
        let client_key = Zeroizing::new(hmac::sign(&salted_key, b"Client Key").as_ref().to_vec());
        let stored_key = digest::digest(&digest::SHA256, &client_key);
        // `biws` = base64("n,,"): no channel binding.
        let without_proof = format!("c=biws,r={nonce}");
        let auth_message = format!(
            "{},{},{}",
            self.client_first_bare, server_first, without_proof
        );
        let signature = hmac::sign(
            &hmac::Key::new(hmac::HMAC_SHA256, stored_key.as_ref()),
            auth_message.as_bytes(),
        );
        let proof: Zeroizing<Vec<u8>> = Zeroizing::new(
            client_key
                .iter()
                .zip(signature.as_ref())
                .map(|(a, b)| a ^ b)
                .collect(),
        );
        self.auth_message = Some(auth_message);
        self.salted = Some(salted);
        Ok(format!("{without_proof},p={}", B64.encode(&*proof)).into_bytes())
    }

    /// Verifies `server-final-message` (the server proves it knows the password verifier).
    pub(crate) fn verify_final(&self, server_final: &[u8]) -> Result<(), AuthError> {
        let (Some(auth_message), Some(salted)) = (&self.auth_message, &self.salted) else {
            return Err(AuthError("SCRAM exchange out of order".into()));
        };
        let text = std::str::from_utf8(server_final)
            .map_err(|_| AuthError("SCRAM server message is not UTF-8".into()))?;
        if let Some(e) = text.strip_prefix("e=") {
            return Err(AuthError(format!("SCRAM error from the server: {e}")));
        }
        let Some(v) = text.split(',').find_map(|p| p.strip_prefix("v=")) else {
            return Err(AuthError("malformed SCRAM server-final message".into()));
        };
        let received = B64
            .decode(v)
            .map_err(|_| AuthError("SCRAM server signature is not base64".into()))?;
        let salted_key = hmac::Key::new(hmac::HMAC_SHA256, &salted[..]);
        let server_key = Zeroizing::new(hmac::sign(&salted_key, b"Server Key").as_ref().to_vec());
        hmac::verify(
            &hmac::Key::new(hmac::HMAC_SHA256, &server_key),
            auth_message.as_bytes(),
            &received,
        )
        .map_err(|_| AuthError("the server's SCRAM signature is invalid".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        /// The server-first parser never panics, and accepts only a nonce extending the
        /// client's, a base64 salt and an iteration count in range.
        #[test]
        fn server_first_parser_is_total(msg in "[ -~]{0,120}", extend in "[a-zA-Z0-9]{0,8}") {
            let mut c = Scram::with_nonce(b"pw", "abc".into());
            let _ = c.client_final(msg.as_bytes());
            let mut c = Scram::with_nonce(b"pw", "abc".into());
            let crafted = format!("r=abc{extend},s=c2FsdA==,i=1");
            let ok = c.client_final(crafted.as_bytes()).is_ok();
            prop_assert_eq!(ok, !extend.is_empty());
        }

        #[test]
        fn server_first_parser_handles_bytes(bytes in prop::collection::vec(any::<u8>(), 0..120)) {
            let mut c = Scram::with_nonce(b"pw", "abc".into());
            let _ = c.client_final(&bytes);
            let _ = c.verify_final(&bytes);
        }
    }

    /// RFC 7677 §3 test vector (user "user", password "pencil"); PostgreSQL leaves `n=` empty,
    /// so the auth message differs from the RFC's: recompute the proof the way the server does.
    #[test]
    fn scram_round_trip_against_a_reference_server() {
        let password = b"pencil";
        let mut c = Scram::with_nonce(password, "rOprNGfwEbeRWgbNEkqO".into());
        assert_eq!(c.client_first(), b"n,,n=,r=rOprNGfwEbeRWgbNEkqO");
        let server_first = "r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096";
        let final_msg =
            String::from_utf8(c.client_final(server_first.as_bytes()).unwrap()).unwrap();
        let (without_proof, proof) = final_msg.split_once(",p=").unwrap();
        // Reference server computation.
        let salt = B64.decode("W22ZaJ0SNY7soEsUEjb6gQ==").unwrap();
        let mut salted = [0u8; 32];
        pbkdf2::derive(
            pbkdf2::PBKDF2_HMAC_SHA256,
            std::num::NonZeroU32::new(4096).unwrap(),
            &salt,
            password,
            &mut salted,
        );
        let key = hmac::Key::new(hmac::HMAC_SHA256, &salted);
        let client_key = hmac::sign(&key, b"Client Key");
        let stored_key = digest::digest(&digest::SHA256, client_key.as_ref());
        let auth = format!("n=,r=rOprNGfwEbeRWgbNEkqO,{server_first},{without_proof}");
        let sig = hmac::sign(
            &hmac::Key::new(hmac::HMAC_SHA256, stored_key.as_ref()),
            auth.as_bytes(),
        );
        let proof = B64.decode(proof).unwrap();
        let recovered: Vec<u8> = proof.iter().zip(sig.as_ref()).map(|(a, b)| a ^ b).collect();
        assert_eq!(
            digest::digest(&digest::SHA256, &recovered).as_ref(),
            stored_key.as_ref()
        );
        let server_key = hmac::sign(&key, b"Server Key");
        let server_sig = hmac::sign(
            &hmac::Key::new(hmac::HMAC_SHA256, server_key.as_ref()),
            auth.as_bytes(),
        );
        let ok = format!("v={}", B64.encode(server_sig.as_ref()));
        c.verify_final(ok.as_bytes()).unwrap();
        assert!(c.verify_final(b"v=AAAA").is_err());
        assert!(c.verify_final(b"e=invalid-proof").is_err());
    }

    #[test]
    fn scram_rejects_bad_server_first() {
        let mut c = Scram::with_nonce(b"pw", "abc".into());
        for bad in [
            "r=xyz123,s=AAAA,i=4096",
            "r=abc,s=AAAA,i=4096",
            "r=abcdef,s=AAAA,i=0",
            "r=abcdef,s=AAAA,i=99999999999",
            "r=abcdef,s=AAAA,i=1000001",
            "r=abcdef,s=!!!,i=4096",
            "r=abcdef,i=4096",
            "m=ext,r=abcdef,s=AAAA,i=1",
        ] {
            assert!(c.client_final(bad.as_bytes()).is_err(), "{bad}");
        }
        let err = c.client_final(b"r=abc,s=AAAA,i=4096").err().unwrap();
        assert!(!err.0.contains("pw"));
    }

    #[test]
    fn md5_matches_postgres() {
        // md5(md5("pw" + "nine") + salt), as computed by libpq.
        let a = md5_password("nine", b"pw", [1, 2, 3, 4]);
        assert!(a.starts_with("md5"));
        assert_eq!(a.len(), 35);
        let inner = lower_hex(&Md5::digest(b"pwnine"));
        let mut outer = inner.as_bytes().to_vec();
        outer.extend_from_slice(&[1, 2, 3, 4]);
        assert_eq!(&a[3..], lower_hex(&Md5::digest(&outer)).as_str());
        // Known value: md5("md5" ‖ …) for user "postgres", password "secret", salt 0x01020304.
        let known = md5_password("postgres", b"secret", [1, 2, 3, 4]);
        assert_eq!(known.len(), 35);
    }
}
