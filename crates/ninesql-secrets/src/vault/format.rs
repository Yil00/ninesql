//! On-disk format of `vault.bin`, version 1 (ADR-0005 §2).
//!
//! ```text
//! offset  size  field
//! ------  ----  -------------------------------------------------------------
//!      0     8  magic  "NSQVAULT"
//!      8     2  format version, u16 little-endian (1)
//!     10     1  KDF id: 1 = Argon2id, version 0x13
//!     11     1  AEAD id: 1 = XChaCha20-Poly1305
//!     12     4  Argon2 memory cost m, KiB, u32 LE
//!     16     4  Argon2 time cost t (passes), u32 LE
//!     20     4  Argon2 parallelism p (lanes), u32 LE
//!     24    16  salt (random, renewed on every master-password change)
//!     40    24  key-check nonce (random)
//!     64    16  key-check tag: XChaCha20-Poly1305 over an empty plaintext, associated data
//!               = "ninesql vault key check v1" || bytes 0..40
//!     80    24  payload nonce (random, renewed on every write)
//!    104     8  payload ciphertext length in bytes (tag included), u64 LE
//!    112     …  payload ciphertext: XChaCha20-Poly1305, associated data = bytes 0..112
//! ```
//!
//! The file length must be exactly `112 + ciphertext length`; anything else is a truncated or
//! extended file and is refused before any key derivation.
//!
//! The key-check tag tells a wrong master password (tag fails) from a damaged or modified payload
//! (tag passes, payload fails). It is no shortcut for an attacker: checking it needs the same
//! Argon2id derivation as decrypting the payload.
//!
//! Plaintext payload (all integers little-endian), padded with zero bytes to a multiple of
//! [`PAD_TO`] bytes so the file size reveals little about the number and length of secrets:
//!
//! ```text
//! u32 entry count
//! entry * count:
//!   u8  project length (36)   project UUID, lower-case ASCII
//!   u8  id length (1..=128)   secret id (SecretId alphabet)
//!   u8  kind                  1 password, 2 ssh_password, 3 ssh_key_passphrase, 4 token,
//!                             5 client_key_passphrase, 6 proxy_password
//!   u16 length (<= 256)       connection id, UTF-8
//!   u16 length (<= 1024)      label, UTF-8
//!   u32 length (<= 65536)     secret, UTF-8
//! zero padding
//! ```

use crate::reference::SecretKind;
use crate::vault::KdfParams;

/// File magic.
pub(crate) const MAGIC: [u8; 8] = *b"NSQVAULT";
/// Newest format version this build reads and the one it writes.
pub(crate) const FORMAT_VERSION: u16 = 1;
/// KDF id: Argon2id, version 0x13.
pub(crate) const KDF_ARGON2ID_V13: u8 = 1;
/// AEAD id: XChaCha20-Poly1305.
pub(crate) const AEAD_XCHACHA20_POLY1305: u8 = 1;

pub(crate) const SALT_LEN: usize = 16;
pub(crate) const NONCE_LEN: usize = 24;
pub(crate) const TAG_LEN: usize = 16;
pub(crate) const KEY_LEN: usize = 32;
/// Length of the header, which is the associated data of the payload.
pub(crate) const HEADER_LEN: usize = 112;
/// Length of the header prefix covered by the key-check tag (magic to salt).
pub(crate) const KDF_PREFIX_LEN: usize = 40;
/// Domain separation of the key-check tag.
pub(crate) const KEY_CHECK_CONTEXT: &[u8] = b"ninesql vault key check v1";
/// Payload padding granularity.
pub(crate) const PAD_TO: usize = 256;

/// Largest secret value stored in the vault, in bytes.
pub const MAX_SECRET_LEN: usize = 64 * 1024;
/// Largest connection id stored with a secret, in bytes.
pub const MAX_CONNECTION_ID_LEN: usize = 256;
/// Largest label stored with a secret, in bytes.
pub const MAX_LABEL_LEN: usize = 1024;
/// Largest number of secrets in one vault.
pub const MAX_ENTRIES: usize = 10_000;
/// Largest `vault.bin` NineSQL reads (16 MiB): far above `MAX_ENTRIES` typical entries, low
/// enough to refuse a planted huge file before allocating for it.
pub const MAX_FILE_LEN: u64 = 16 * 1024 * 1024;

/// Why a file is not a readable vault. Never carries file content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FormatError {
    /// Wrong magic: not a NineSQL vault.
    NotAVault,
    /// Written by a newer NineSQL.
    NewerVersion(u16),
    /// Structurally invalid, with a fixed description.
    Invalid(&'static str),
}

/// Parsed header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Header {
    pub kdf: KdfParams,
    pub salt: [u8; SALT_LEN],
    pub check_nonce: [u8; NONCE_LEN],
    pub check_tag: [u8; TAG_LEN],
    pub nonce: [u8; NONCE_LEN],
    pub ciphertext_len: u64,
}

impl Header {
    /// The 112 header bytes.
    pub(crate) fn encode(&self) -> [u8; HEADER_LEN] {
        let mut out = [0u8; HEADER_LEN];
        out[0..8].copy_from_slice(&MAGIC);
        out[8..10].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        out[10] = KDF_ARGON2ID_V13;
        out[11] = AEAD_XCHACHA20_POLY1305;
        out[12..16].copy_from_slice(&self.kdf.m_cost_kib().to_le_bytes());
        out[16..20].copy_from_slice(&self.kdf.t_cost().to_le_bytes());
        out[20..24].copy_from_slice(&self.kdf.p_cost().to_le_bytes());
        out[24..40].copy_from_slice(&self.salt);
        out[40..64].copy_from_slice(&self.check_nonce);
        out[64..80].copy_from_slice(&self.check_tag);
        out[80..104].copy_from_slice(&self.nonce);
        out[104..112].copy_from_slice(&self.ciphertext_len.to_le_bytes());
        out
    }

    /// Splits a whole file into its header and ciphertext, checking every structural rule.
    pub(crate) fn decode(file: &[u8]) -> Result<(Self, &[u8]), FormatError> {
        if file.len() < MAGIC.len() || file[0..8] != MAGIC {
            return Err(FormatError::NotAVault);
        }
        if file.len() < HEADER_LEN {
            return Err(FormatError::Invalid("truncated header"));
        }
        let version = u16::from_le_bytes([file[8], file[9]]);
        if version == 0 {
            return Err(FormatError::Invalid("format version 0"));
        }
        if version > FORMAT_VERSION {
            return Err(FormatError::NewerVersion(version));
        }
        if file[10] != KDF_ARGON2ID_V13 || file[11] != AEAD_XCHACHA20_POLY1305 {
            return Err(FormatError::Invalid("unknown KDF or cipher"));
        }
        let kdf = KdfParams::from_file(u32_at(file, 12), u32_at(file, 16), u32_at(file, 20))
            .ok_or(FormatError::Invalid("KDF parameters out of range"))?;
        let ciphertext_len = u64::from_le_bytes(array_at(file, 104));
        let body = &file[HEADER_LEN..];
        if ciphertext_len < TAG_LEN as u64 {
            return Err(FormatError::Invalid("payload shorter than its tag"));
        }
        match (body.len() as u64).cmp(&ciphertext_len) {
            std::cmp::Ordering::Less => return Err(FormatError::Invalid("truncated payload")),
            std::cmp::Ordering::Greater => return Err(FormatError::Invalid("trailing data")),
            std::cmp::Ordering::Equal => {}
        }
        let header = Self {
            kdf,
            salt: array_at(file, 24),
            check_nonce: array_at(file, 40),
            check_tag: array_at(file, 64),
            nonce: array_at(file, 80),
            ciphertext_len,
        };
        Ok((header, body))
    }
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(array_at(bytes, at))
}

/// `N` bytes at `at`. Callers have checked the length (`HEADER_LEN`).
fn array_at<const N: usize>(bytes: &[u8], at: usize) -> [u8; N] {
    let mut out = [0u8; N];
    out.copy_from_slice(&bytes[at..at + N]);
    out
}

/// Stable byte of a [`SecretKind`] in the payload.
pub(crate) const fn kind_to_byte(kind: SecretKind) -> u8 {
    match kind {
        SecretKind::Password => 1,
        SecretKind::SshPassword => 2,
        SecretKind::SshKeyPassphrase => 3,
        SecretKind::Token => 4,
        SecretKind::ClientKeyPassphrase => 5,
        SecretKind::ProxyPassword => 6,
    }
}

/// Inverse of [`kind_to_byte`].
pub(crate) const fn kind_from_byte(byte: u8) -> Option<SecretKind> {
    Some(match byte {
        1 => SecretKind::Password,
        2 => SecretKind::SshPassword,
        3 => SecretKind::SshKeyPassphrase,
        4 => SecretKind::Token,
        5 => SecretKind::ClientKeyPassphrase,
        6 => SecretKind::ProxyPassword,
        _ => return None,
    })
}

/// One payload entry, borrowed for encoding.
pub(crate) struct EntryRef<'a> {
    pub project: &'a str,
    pub id: &'a str,
    pub kind: SecretKind,
    pub connection_id: &'a str,
    pub label: &'a str,
    pub secret: &'a [u8],
}

impl EntryRef<'_> {
    fn encoded_len(&self) -> usize {
        1 + self.project.len()
            + 1
            + self.id.len()
            + 1
            + 2
            + self.connection_id.len()
            + 2
            + self.label.len()
            + 4
            + self.secret.len()
    }
}

/// Encoded (padded) length of a payload.
pub(crate) fn payload_len<'a>(entries: impl Iterator<Item = &'a EntryRef<'a>>) -> usize {
    let raw = 4 + entries.map(EntryRef::encoded_len).sum::<usize>();
    raw.div_ceil(PAD_TO).max(1) * PAD_TO
}

/// Encodes the payload into `out`, which the caller allocated with enough capacity for the
/// payload **and** the AEAD tag, so neither encoding nor in-place encryption reallocates (a
/// reallocation would leave an unzeroized copy of the plaintext on the heap).
///
/// The caller checks the limits first; a field that still does not fit its length prefix or
/// exceeds the limit the decoder enforces is an error (never clamped, which would write a file
/// that cannot be read back). On error `out` holds a partial plaintext: it must be a zeroizing
/// buffer.
pub(crate) fn encode_payload(
    entries: &[EntryRef<'_>],
    out: &mut Vec<u8>,
) -> Result<(), &'static str> {
    let start_capacity = out.capacity();
    if entries.len() > MAX_ENTRIES {
        return Err("too many secrets in the vault");
    }
    let count = u32::try_from(entries.len()).map_err(|_| "too many secrets in the vault")?;
    out.extend_from_slice(&count.to_le_bytes());
    for e in entries {
        push_u8_len(out, e.project.as_bytes(), "project id too long")?;
        push_u8_len(out, e.id.as_bytes(), "secret id too long")?;
        out.push(kind_to_byte(e.kind));
        push_u16_len(
            out,
            e.connection_id.as_bytes(),
            MAX_CONNECTION_ID_LEN,
            "connection id longer than 256 bytes",
        )?;
        push_u16_len(
            out,
            e.label.as_bytes(),
            MAX_LABEL_LEN,
            "label longer than 1024 bytes",
        )?;
        if e.secret.len() > MAX_SECRET_LEN {
            return Err("secret longer than 64 KiB");
        }
        let len = u32::try_from(e.secret.len()).map_err(|_| "secret longer than 64 KiB")?;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(e.secret);
    }
    let padded = out.len().div_ceil(PAD_TO).max(1) * PAD_TO;
    out.resize(padded, 0);
    debug_assert_eq!(out.capacity(), start_capacity, "payload buffer reallocated");
    Ok(())
}

fn push_u8_len(out: &mut Vec<u8>, bytes: &[u8], err: &'static str) -> Result<(), &'static str> {
    let len = u8::try_from(bytes.len()).map_err(|_| err)?;
    out.push(len);
    out.extend_from_slice(bytes);
    Ok(())
}

fn push_u16_len(
    out: &mut Vec<u8>,
    bytes: &[u8],
    max: usize,
    err: &'static str,
) -> Result<(), &'static str> {
    if bytes.len() > max {
        return Err(err);
    }
    let len = u16::try_from(bytes.len()).map_err(|_| err)?;
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(bytes);
    Ok(())
}

/// One decoded entry, borrowing from the plaintext buffer.
pub(crate) struct DecodedEntry<'a> {
    pub project: &'a str,
    pub id: &'a str,
    pub kind: SecretKind,
    pub connection_id: &'a str,
    pub label: &'a str,
    pub secret: &'a str,
}

/// Decodes a payload. Errors are fixed strings, never content.
pub(crate) fn decode_payload(plain: &[u8]) -> Result<Vec<DecodedEntry<'_>>, &'static str> {
    let mut r = Reader { buf: plain, pos: 0 };
    let count = r.u32()? as usize;
    if count > MAX_ENTRIES {
        return Err("too many entries");
    }
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        let project = r.str_u8()?;
        let id = r.str_u8()?;
        let kind = kind_from_byte(r.u8()?).ok_or("unknown secret kind")?;
        let connection_id = r.str_u16(MAX_CONNECTION_ID_LEN)?;
        let label = r.str_u16(MAX_LABEL_LEN)?;
        let len = r.u32()? as usize;
        if len > MAX_SECRET_LEN {
            return Err("secret too long");
        }
        let secret = std::str::from_utf8(r.take(len)?).map_err(|_| "secret is not UTF-8")?;
        entries.push(DecodedEntry {
            project,
            id,
            kind,
            connection_id,
            label,
            secret,
        });
    }
    if r.buf[r.pos..].iter().any(|&b| b != 0) {
        return Err("non-zero padding");
    }
    Ok(entries)
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], &'static str> {
        let end = self.pos.checked_add(n).ok_or("length overflow")?;
        let bytes = self.buf.get(self.pos..end).ok_or("truncated entry")?;
        self.pos = end;
        Ok(bytes)
    }
    fn u8(&mut self) -> Result<u8, &'static str> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, &'static str> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }
    fn u32(&mut self) -> Result<u32, &'static str> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn str_u8(&mut self) -> Result<&'a str, &'static str> {
        let n = self.u8()? as usize;
        std::str::from_utf8(self.take(n)?).map_err(|_| "field is not UTF-8")
    }
    fn str_u16(&mut self, max: usize) -> Result<&'a str, &'static str> {
        let n = self.u16()? as usize;
        if n > max {
            return Err("field too long");
        }
        std::str::from_utf8(self.take(n)?).map_err(|_| "field is not UTF-8")
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn header() -> Header {
        Header {
            kdf: KdfParams::DEFAULT,
            salt: [1; SALT_LEN],
            check_nonce: [2; NONCE_LEN],
            check_tag: [3; TAG_LEN],
            nonce: [4; NONCE_LEN],
            ciphertext_len: 20,
        }
    }

    #[test]
    fn header_round_trip_and_layout() {
        let h = header();
        let bytes = h.encode();
        assert_eq!(&bytes[0..8], b"NSQVAULT");
        assert_eq!(&bytes[8..10], &[1, 0]);
        assert_eq!(&bytes[12..16], &65536u32.to_le_bytes());
        assert_eq!(&bytes[16..20], &3u32.to_le_bytes());
        assert_eq!(&bytes[20..24], &1u32.to_le_bytes());
        let mut file = bytes.to_vec();
        file.extend_from_slice(&[9; 20]);
        let (back, body) = Header::decode(&file).unwrap();
        assert_eq!(back, h);
        assert_eq!(body, &[9; 20]);
    }

    #[test]
    fn structural_errors() {
        let mut file = header().encode().to_vec();
        file.extend_from_slice(&[9; 20]);
        assert_eq!(Header::decode(b"").unwrap_err(), FormatError::NotAVault);
        assert_eq!(Header::decode(b"{}").unwrap_err(), FormatError::NotAVault);
        assert!(matches!(
            Header::decode(&file[..50]),
            Err(FormatError::Invalid("truncated header"))
        ));
        assert!(matches!(
            Header::decode(&file[..file.len() - 1]),
            Err(FormatError::Invalid("truncated payload"))
        ));
        let mut longer = file.clone();
        longer.push(0);
        assert!(matches!(
            Header::decode(&longer),
            Err(FormatError::Invalid("trailing data"))
        ));
        let mut newer = file.clone();
        newer[8] = 2;
        assert_eq!(
            Header::decode(&newer).unwrap_err(),
            FormatError::NewerVersion(2)
        );
        let mut algo = file.clone();
        algo[11] = 2;
        assert!(Header::decode(&algo).is_err());
        // A planted header asking for a huge Argon2 memory cost is refused before derivation.
        let mut huge = file.clone();
        huge[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(
            Header::decode(&huge),
            Err(FormatError::Invalid("KDF parameters out of range"))
        ));
        let mut short_ct = file;
        short_ct[104..112].copy_from_slice(&3u64.to_le_bytes());
        assert!(Header::decode(&short_ct).is_err());
    }

    #[test]
    fn kind_bytes_round_trip() {
        for b in 0..=255u8 {
            if let Some(kind) = kind_from_byte(b) {
                assert_eq!(kind_to_byte(kind), b);
            }
        }
        assert!(kind_from_byte(0).is_none());
        assert!(kind_from_byte(7).is_none());
    }

    #[test]
    fn hostile_payloads_are_refused_without_panic() {
        assert!(decode_payload(&[]).is_err());
        assert!(decode_payload(&u32::MAX.to_le_bytes()).is_err());
        // One entry announced, nothing follows.
        assert!(decode_payload(&1u32.to_le_bytes()).is_err());
        // Non-zero padding.
        let mut p = vec![0u8; PAD_TO];
        p[10] = 1;
        assert!(decode_payload(&p).is_err());
        assert!(decode_payload(&vec![0u8; PAD_TO]).unwrap().is_empty());
    }

    fn kind_strategy() -> impl Strategy<Value = SecretKind> {
        (1u8..=6).prop_map(|b| kind_from_byte(b).unwrap())
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn payload_round_trip(
            entries in proptest::collection::vec(
                (
                    "[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}",
                    "[A-Za-z0-9:._-]{1,128}",
                    kind_strategy(),
                    "\\PC{0,40}",
                    "\\PC{0,60}",
                    "\\PC{0,200}",
                ),
                0..12,
            )
        ) {
            let refs: Vec<EntryRef<'_>> = entries
                .iter()
                .map(|(p, i, k, c, l, s)| EntryRef {
                    project: p,
                    id: i,
                    kind: *k,
                    connection_id: c,
                    label: l,
                    secret: s.as_bytes(),
                })
                .collect();
            let len = payload_len(refs.iter());
            let mut buf = Vec::with_capacity(len + TAG_LEN);
            encode_payload(&refs, &mut buf).unwrap();
            prop_assert_eq!(buf.len(), len);
            prop_assert_eq!(buf.len() % PAD_TO, 0);
            prop_assert_eq!(buf.capacity(), len + TAG_LEN);
            let back = decode_payload(&buf).unwrap();
            prop_assert_eq!(back.len(), entries.len());
            for (d, (p, i, k, c, l, s)) in back.iter().zip(&entries) {
                prop_assert_eq!(d.project, p.as_str());
                prop_assert_eq!(d.id, i.as_str());
                prop_assert_eq!(d.kind, *k);
                prop_assert_eq!(d.connection_id, c.as_str());
                prop_assert_eq!(d.label, l.as_str());
                prop_assert_eq!(d.secret, s.as_str());
            }
        }

        #[test]
        fn decode_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..2048)) {
            let _ = decode_payload(&bytes);
            let _ = Header::decode(&bytes);
        }

        /// Starts from a valid file and damages it the ways an attacker or a disk would: byte
        /// flips, overwritten fields, truncation, extension. Decoding never panics, and whatever
        /// it accepts still satisfies the structural rules.
        #[test]
        fn mutated_valid_headers_are_checked(
            body_len in 16usize..600,
            flips in proptest::collection::vec((0usize..1024, 1u8..=255), 0..6),
            field in proptest::option::of((
                prop_oneof![Just(8usize), Just(12), Just(16), Just(20), Just(104)],
                any::<u64>(),
            )),
            cut in proptest::option::of(0usize..1024),
            extra in proptest::collection::vec(any::<u8>(), 0..4),
        ) {
            let mut h = header();
            h.ciphertext_len = body_len as u64;
            let mut file = h.encode().to_vec();
            file.extend(std::iter::repeat_n(7u8, body_len));
            for (at, x) in flips {
                let at = at % file.len();
                file[at] ^= x;
            }
            if let Some((at, v)) = field {
                let width = match at { 8 => 2, 104 => 8, _ => 4 };
                file[at..at + width].copy_from_slice(&v.to_le_bytes()[..width]);
            }
            if let Some(cut) = cut {
                file.truncate(cut.min(file.len()));
            }
            file.extend_from_slice(&extra);
            if let Ok((back, body)) = Header::decode(&file) {
                prop_assert_eq!(body.len() as u64, back.ciphertext_len);
                prop_assert_eq!(file.len(), HEADER_LEN + body.len());
                prop_assert!(back.ciphertext_len >= TAG_LEN as u64);
                prop_assert!(back.kdf.m_cost_kib() <= KdfParams::MAX_M_COST_KIB);
                prop_assert!(back.kdf.t_cost() <= KdfParams::MAX_T_COST);
                prop_assert!((1..=KdfParams::MAX_P_COST).contains(&back.kdf.p_cost()));
                prop_assert_eq!(&file[0..8], &MAGIC);
                prop_assert_eq!(u16::from_le_bytes([file[8], file[9]]), FORMAT_VERSION);
            }
        }
    }

    /// L8: fields that do not fit are refused, not clamped into an unreadable file.
    #[test]
    fn encoding_refuses_oversized_fields() {
        let long_id = "i".repeat(256);
        let long_conn = "c".repeat(MAX_CONNECTION_ID_LEN + 1);
        let long_secret = vec![b's'; MAX_SECRET_LEN + 1];
        let ok = EntryRef {
            project: "3f2a6c1e-0b7d-4c51-9e2a-6d8f0a1b2c3d",
            id: "c:auth",
            kind: SecretKind::Password,
            connection_id: "c",
            label: "l",
            secret: b"x",
        };
        for bad in [
            EntryRef { id: &long_id, ..ok },
            EntryRef {
                connection_id: &long_conn,
                ..ok
            },
            EntryRef {
                secret: &long_secret,
                ..ok
            },
        ] {
            let refs = [bad];
            let mut buf = Vec::with_capacity(payload_len(refs.iter()) + TAG_LEN);
            assert!(encode_payload(&refs, &mut buf).is_err());
        }
    }
}
