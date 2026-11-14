//! Secret values handed to a driver at connect time (I1, ADR-0005).
//!
//! `ninesql-secrets` resolves a stored secret reference (keyring, vault or prompt) into a
//! [`Secret`] just before [`Driver::connect`](crate::Driver::connect) and moves it into the
//! [`ConnectParams`](crate::ConnectParams). The driver uses it for the handshake and lets it
//! drop: the bytes are zeroized on drop.
//!
//! [`Secret`] deliberately implements neither `Display`, `Clone`, `PartialEq`, `serde`
//! traits nor a revealing `Debug`; the only way to read it is [`Secret::expose`], which
//! makes every use greppable in review.

use std::fmt;

use zeroize::Zeroizing;

/// A password, passphrase or token, zeroized on drop and never printed.
pub struct Secret(Zeroizing<String>);

impl Secret {
    /// Wraps a secret value. The argument is moved, so no copy stays with the caller.
    #[must_use]
    pub fn new(value: String) -> Self {
        Self(Zeroizing::new(value))
    }

    /// Reads the secret. Call it only where the value is sent to the engine or the TLS stack;
    /// never format, log or store the returned slice.
    #[must_use]
    pub fn expose(&self) -> &str {
        self.0.as_str()
    }

    /// `true` when the secret is the empty string.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl From<String> for Secret {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_shows_value() {
        let s = Secret::new("hunter2-very-secret".to_owned());
        let dbg = format!("{s:?}");
        assert!(!dbg.contains("hunter2"));
        assert_eq!(dbg, "Secret(<redacted>)");
        let alt = format!("{s:#?}");
        assert!(!alt.contains("hunter2"));
    }

    #[test]
    fn expose_returns_value() {
        let s = Secret::from("abc".to_owned());
        assert_eq!(s.expose(), "abc");
        assert!(!s.is_empty());
        assert!(Secret::new(String::new()).is_empty());
    }
}
