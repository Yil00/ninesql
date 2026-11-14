//! DBeaver Community's `credentials-config.json` (docs/06-dbeaver-interop.md, ADR-0006 §4).
//!
//! Scheme, verified on the five P0-E fixture versions (22.3.5 to 26.2.4):
//! - AES-128-CBC, PKCS#5/7 padding;
//! - the fixed key from DBeaver Community's source code, identical for every installation: this
//!   is obfuscation, not protection, which is why NineSQL moves the secrets to the keyring
//!   (ADR-0005);
//! - file = 16-byte IV followed by the ciphertext;
//! - plaintext = compact JSON `{ "<connection id>": { "#connection": { "user", "password" },
//!   "network/<handler id>": { "user", "password" } } }`.
//!
//! A file that does not decrypt to that structure (DBeaver PRO master password or secure storage,
//! a corrupted file, a future format) is reported as [`CredentialsError`]; the connections are
//! then imported without passwords.
//!
//! Secret hygiene (I1): the decrypted bytes live in a [`Zeroizing`] buffer; passwords are
//! deserialized straight into exact-capacity strings and wrapped in [`SecretString`]; values of
//! unexpected keys are zeroized and dropped. Nothing here logs, formats or returns a value.
//! Known limit: `serde_json` copies strings that contain escape sequences through an internal
//! scratch buffer that is not zeroized.

use std::collections::BTreeMap;
use std::fmt;

use aes::cipher::{BlockDecryptMut, KeyIvInit, block_padding::Pkcs7};
use ninesql_secrets::SecretString;
use serde::Deserialize;
use serde::de::{self, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use zeroize::{Zeroize, Zeroizing};

/// Fixed AES-128 key of DBeaver Community's `credentials-config.json`. Public: it is in
/// DBeaver's source code and identical for every installation.
const DBEAVER_KEY: [u8; 16] = [
    0xba, 0xbb, 0x4a, 0x9f, 0x77, 0x4a, 0xb8, 0x53, 0xc9, 0x6c, 0x2d, 0x65, 0x3d, 0xfe, 0x54, 0x4a,
];

type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;

/// Section of the database credentials.
pub const CONNECTION_SECTION: &str = "#connection";
/// Section of the SSH tunnel handler.
pub const SSH_SECTION: &str = "network/ssh_tunnel";

/// Why a `credentials-config.json` could not be used. Never carries content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum CredentialsError {
    /// The file is empty.
    #[error("the file is empty")]
    Empty,
    /// The size or the padding does not match DBeaver Community's scheme: the file is protected
    /// by a DBeaver PRO master password or secure storage, or it is corrupted.
    #[error(
        "not in DBeaver Community's format: protected by a DBeaver PRO master password, \
         or corrupted"
    )]
    NotCommunityFormat,
    /// The file decrypts, but not to JSON.
    #[error("the decrypted content is not JSON (corrupted or unknown format)")]
    NotJson,
    /// The file decrypts to JSON whose top level is not an object.
    #[error("the decrypted content has an unknown structure")]
    UnexpectedStructure,
}

/// Saved values of one section (`#connection`, `network/ssh_tunnel`…).
#[derive(Default)]
pub struct Section {
    /// `user`. A user name is not a secret (the NineSQL model stores it in clear).
    pub user: Option<String>,
    /// `password`. For `network/ssh_tunnel` with public-key auth, this is the key passphrase.
    pub password: Option<SecretString>,
    /// Other keys of the section: their values were zeroized and dropped (keys only).
    pub other_keys: Vec<String>,
}

impl fmt::Debug for Section {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Section")
            .field("user", &self.user.as_ref().map(|_| ".."))
            .field("password", &self.password.as_ref().map(|_| "[REDACTED]"))
            .field("other_keys", &self.other_keys)
            .finish()
    }
}

/// Decrypted credentials of one connection, by section name.
#[derive(Debug, Default)]
pub struct ConnectionCredentials {
    /// Sections by name.
    pub sections: BTreeMap<String, Section>,
    /// Section names whose value was not an object (dropped).
    pub invalid_sections: Vec<String>,
}

/// Decrypted `credentials-config.json`.
#[derive(Debug, Default)]
pub struct Credentials {
    /// Entries by DBeaver connection id.
    pub connections: BTreeMap<String, ConnectionCredentials>,
    /// Connection ids whose value was not an object (dropped).
    pub invalid_entries: Vec<String>,
}

impl Credentials {
    /// Number of saved passwords (all sections).
    #[must_use]
    pub fn password_count(&self) -> usize {
        self.connections
            .values()
            .flat_map(|c| c.sections.values())
            .filter(|s| s.password.is_some())
            .count()
    }
}

/// Decrypts and parses the bytes of a `credentials-config.json`.
///
/// # Errors
/// [`CredentialsError`] when the file is not in DBeaver Community's format.
pub fn decrypt_credentials(bytes: &[u8]) -> Result<Credentials, CredentialsError> {
    if bytes.is_empty() {
        return Err(CredentialsError::Empty);
    }
    // 16-byte IV + at least one ciphertext block, whole blocks only.
    if bytes.len() < 32 || !bytes.len().is_multiple_of(16) {
        return Err(CredentialsError::NotCommunityFormat);
    }
    let (iv, ciphertext) = bytes.split_at(16);
    let mut buf = Zeroizing::new(ciphertext.to_vec());
    let decryptor = Aes128CbcDec::new_from_slices(&DBEAVER_KEY, iv)
        .map_err(|_| CredentialsError::NotCommunityFormat)?;
    let plaintext = decryptor
        .decrypt_padded_mut::<Pkcs7>(&mut buf)
        .map_err(|_| CredentialsError::NotCommunityFormat)?;
    parse_plaintext(plaintext)
}

/// Parses decrypted content. Separate from [`decrypt_credentials`] for tests and fuzzing.
pub(crate) fn parse_plaintext(plaintext: &[u8]) -> Result<Credentials, CredentialsError> {
    let top: Level<Level<Level<CredValue>>> = serde_json::from_slice(plaintext).map_err(|e| {
        if e.is_data() {
            CredentialsError::UnexpectedStructure
        } else {
            CredentialsError::NotJson
        }
    })?;
    let Some(entries) = top.0 else {
        return Err(CredentialsError::UnexpectedStructure);
    };
    let mut out = Credentials::default();
    for (id, sections) in entries {
        let Some(sections) = sections.0 else {
            out.invalid_entries.push(id);
            continue;
        };
        let mut conn = ConnectionCredentials::default();
        for (name, values) in sections {
            let Some(values) = values.0 else {
                conn.invalid_sections.push(name);
                continue;
            };
            let mut section = Section::default();
            for (key, value) in values {
                match (key.as_str(), value) {
                    ("user", CredValue::Text(text)) => section.user = Some(take(text)),
                    ("password", CredValue::Text(text)) => {
                        section.password = Some(into_secret(take(text)));
                    }
                    // Wrong type, or an unknown key: dropped (zeroized by `CredValue`'s drop).
                    (_, _) => section.other_keys.push(key),
                }
            }
            conn.sections.insert(name, section);
        }
        out.connections.insert(id, conn);
    }
    Ok(out)
}

fn take(mut text: Zeroizing<String>) -> String {
    std::mem::take(&mut *text)
}

/// Wraps a string in a [`SecretString`] without leaving an unzeroized copy behind:
/// `SecretString::from(String)` shrinks the allocation (a reallocation, which copies) when the
/// capacity exceeds the length.
pub(crate) fn into_secret(mut s: String) -> SecretString {
    if s.capacity() == s.len() {
        return SecretString::from(s);
    }
    let mut exact = String::with_capacity(s.len());
    exact.push_str(&s);
    s.zeroize();
    SecretString::from(exact)
}

/// A JSON object of `T`, or `None` when the value was anything else (skipped).
struct Level<T>(Option<BTreeMap<String, T>>);

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Level<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V<T>(std::marker::PhantomData<T>);
        impl<'de, T: Deserialize<'de>> Visitor<'de> for V<T> {
            type Value = Level<T>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut out = BTreeMap::new();
                while let Some(key) = map.next_key::<String>()? {
                    let value = map.next_value::<T>()?;
                    // A duplicate key replaces (and drops, so zeroizes) the earlier value.
                    out.insert(key, value);
                }
                Ok(Level(Some(out)))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                while seq.next_element::<CredValue>()?.is_some() {}
                Ok(Level(None))
            }
            fn visit_str<E: de::Error>(self, _: &str) -> Result<Self::Value, E> {
                Ok(Level(None))
            }
            fn visit_bool<E: de::Error>(self, _: bool) -> Result<Self::Value, E> {
                Ok(Level(None))
            }
            fn visit_i64<E: de::Error>(self, _: i64) -> Result<Self::Value, E> {
                Ok(Level(None))
            }
            fn visit_u64<E: de::Error>(self, _: u64) -> Result<Self::Value, E> {
                Ok(Level(None))
            }
            fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> {
                Ok(Level(None))
            }
            fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(Level(None))
            }
        }
        deserializer.deserialize_any(V(std::marker::PhantomData))
    }
}

/// A leaf value: a string kept in a zeroizing buffer, or anything else (discarded).
enum CredValue {
    Text(Zeroizing<String>),
    Other,
}

impl<'de> Deserialize<'de> for CredValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = CredValue;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                let mut s = String::with_capacity(v.len());
                s.push_str(v);
                Ok(CredValue::Text(Zeroizing::new(s)))
            }
            fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
                Ok(CredValue::Text(Zeroizing::new(v)))
            }
            fn visit_bool<E: de::Error>(self, _: bool) -> Result<Self::Value, E> {
                Ok(CredValue::Other)
            }
            fn visit_i64<E: de::Error>(self, _: i64) -> Result<Self::Value, E> {
                Ok(CredValue::Other)
            }
            fn visit_u64<E: de::Error>(self, _: u64) -> Result<Self::Value, E> {
                Ok(CredValue::Other)
            }
            fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> {
                Ok(CredValue::Other)
            }
            fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(CredValue::Other)
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                while seq.next_element::<CredValue>()?.is_some() {}
                Ok(CredValue::Other)
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                while map.next_key::<IgnoredAny>()?.is_some() {
                    map.next_value::<CredValue>()?;
                }
                Ok(CredValue::Other)
            }
        }
        deserializer.deserialize_any(V)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ninesql_secrets::ExposeSecret;

    #[test]
    fn plaintext_structure() {
        let creds = parse_plaintext(
            br##"{"c1":{"#connection":{"user":"nine_user","password":"fixture-a","x":1},
                "network/ssh_tunnel":{"password":"fixture-b"},"bad":3},"c2":[1,{"a":"b"}]}"##,
        );
        let Ok(creds) = creds else {
            panic!("expected credentials");
        };
        assert_eq!(creds.invalid_entries, ["c2"]);
        let c1 = &creds.connections["c1"];
        assert_eq!(c1.invalid_sections, ["bad"]);
        let conn = &c1.sections[CONNECTION_SECTION];
        assert_eq!(conn.user.as_deref(), Some("nine_user"));
        assert_eq!(
            conn.password
                .as_ref()
                .map(|p| p.expose_secret() == "fixture-a"),
            Some(true)
        );
        assert_eq!(conn.other_keys, ["x"]);
        assert_eq!(creds.password_count(), 2);
        let dbg = format!("{creds:?}");
        assert!(!dbg.contains("fixture-"), "Debug leaked a secret");
        assert!(!dbg.contains("nine_user"));
    }

    #[test]
    fn rejects_non_community_files() {
        assert_eq!(
            decrypt_credentials(b"").err(),
            Some(CredentialsError::Empty)
        );
        assert_eq!(
            decrypt_credentials(&[0u8; 31]).err(),
            Some(CredentialsError::NotCommunityFormat)
        );
        assert_eq!(
            decrypt_credentials(br##"{"c1":{"#connection":{"user":"u"}}}"##).err(),
            Some(CredentialsError::NotCommunityFormat)
        );
        assert_eq!(
            parse_plaintext(b"[1,2]").err(),
            Some(CredentialsError::UnexpectedStructure)
        );
        assert_eq!(parse_plaintext(b"{").err(), Some(CredentialsError::NotJson));
    }

    #[test]
    fn into_secret_keeps_value() {
        let mut s = String::with_capacity(64);
        s.push_str("fixture-x");
        assert_eq!(into_secret(s).expose_secret(), "fixture-x");
        assert_eq!(
            into_secret("fixture-y".to_owned()).expose_secret(),
            "fixture-y"
        );
    }
}
