// Modified for NineSQL: `compound_text_to_utf8` rewritten so that decoding never fails or panics
// (unescaped text is ISO 8859-1, GB 2312 / KS C 5601 / JIS X 0208 decoded, U+FFFD otherwise);
// regression tests added. See docs/audits/gpui-snapshot.md in the NineSQL repository.

//! Currently only support utf8 mode for encoding; decoding follows COMPOUND_TEXT.

#![no_std]

extern crate alloc;

#[cfg(feature = "std")]
extern crate std;

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

#[cfg(feature = "std")]
use std::io::{self, Write};

const UTF8_START: &[u8] = &[0x1B, 0x25, 0x47];
const UTF8_END: &[u8] = &[0x1B, 0x25, 0x40];

/// Wrapper for reduce allocation
#[derive(Clone, Copy)]
#[repr(transparent)]
pub struct CText<'s> {
    utf8: &'s str,
}

impl<'s> fmt::Debug for CText<'s> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.utf8)
    }
}

impl<'s> fmt::Display for CText<'s> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.utf8)
    }
}

impl<'s> CText<'s> {
    pub const fn new(utf8: &'s str) -> Self {
        Self { utf8 }
    }

    pub const fn len(self) -> usize {
        self.utf8.len() + UTF8_START.len() + UTF8_END.len()
    }

    #[cfg(feature = "std")]
    pub fn write(self, mut out: impl Write) -> io::Result<usize> {
        let mut writed = 0;
        writed += out.write(UTF8_START)?;
        writed += out.write(self.utf8.as_bytes())?;
        writed += out.write(UTF8_END)?;
        Ok(writed)
    }
}

/// Encoding utf8 to COMPOUND_TEXT with utf8 escape
pub fn utf8_to_compound_text(text: &str) -> Vec<u8> {
    let mut ret = Vec::with_capacity(text.len() + 6);
    ret.extend_from_slice(UTF8_START);
    ret.extend_from_slice(text.as_bytes());
    ret.extend_from_slice(UTF8_END);
    ret
}

#[derive(Debug, Clone)]
pub enum DecodeError {
    InvalidEncoding,
    UnsupportedEncoding,
    Utf8Error(alloc::string::FromUtf8Error),
}

impl From<alloc::string::FromUtf8Error> for DecodeError {
    fn from(err: alloc::string::FromUtf8Error) -> Self {
        DecodeError::Utf8Error(err)
    }
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidEncoding => write!(f, "Invalid compound text"),
            Self::UnsupportedEncoding => write!(f, "This encoding is not supported yet"),
            Self::Utf8Error(e) => write!(f, "Not a valid utf8 {}", e),
        }
    }
}

/// Character set designated to GL (bytes 0x21-0x7E) or GR (bytes 0xA0-0xFF).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Charset {
    /// ASCII / ISO 646 (GL default).
    Ascii,
    /// Right half of ISO 8859-1 (GR default).
    Latin1,
    /// Right half of another single-byte ISO 8859 part.
    Iso8859(&'static encoding_rs::Encoding),
    /// JIS X 0201 katakana.
    Katakana,
    /// A 94x94 set (GB 2312, JIS X 0208, KS C 5601), decoded through its EUC form.
    Double(&'static encoding_rs::Encoding),
    /// A set this decoder does not know: its characters become U+FFFD.
    Unknown,
}

/// 96-character sets designated to GR with `ESC - F`.
fn iso8859(final_byte: u8) -> Charset {
    match final_byte {
        b'A' => Charset::Latin1,
        b'B' => Charset::Iso8859(encoding_rs::ISO_8859_2),
        b'C' => Charset::Iso8859(encoding_rs::ISO_8859_3),
        b'D' => Charset::Iso8859(encoding_rs::ISO_8859_4),
        b'F' => Charset::Iso8859(encoding_rs::ISO_8859_7),
        b'G' => Charset::Iso8859(encoding_rs::ISO_8859_6),
        b'H' => Charset::Iso8859(encoding_rs::ISO_8859_8),
        b'L' => Charset::Iso8859(encoding_rs::ISO_8859_5),
        b'M' => Charset::Iso8859(encoding_rs::WINDOWS_1254), // ISO 8859-9 (superset)
        b'V' => Charset::Iso8859(encoding_rs::ISO_8859_10),
        b'b' => Charset::Iso8859(encoding_rs::ISO_8859_15),
        _ => Charset::Unknown,
    }
}

/// 94x94 sets designated with `ESC $ ( F` (GL) or `ESC $ ) F` (GR).
fn double(final_byte: u8) -> Charset {
    match final_byte {
        b'A' => Charset::Double(encoding_rs::GBK), // GB 2312, EUC-CN form
        b'B' => Charset::Double(encoding_rs::EUC_JP), // JIS X 0208
        b'C' => Charset::Double(encoding_rs::EUC_KR), // KS C 5601
        _ => Charset::Unknown,
    }
}

fn push_decoded(out: &mut String, encoding: &'static encoding_rs::Encoding, bytes: &[u8]) {
    out.push_str(&encoding.decode_without_bom_handling(bytes).0);
}

/// Decodes one GL or GR character starting at `bytes[0]` in `charset`; returns the number of
/// bytes used (at least 1).
fn decode_char(out: &mut String, charset: Charset, bytes: &[u8]) -> usize {
    let b = bytes[0];
    match charset {
        Charset::Ascii => out.push(char::from(b & 0x7F)),
        Charset::Latin1 => out.push(char::from(b | 0x80)),
        Charset::Iso8859(encoding) => push_decoded(out, encoding, &[b | 0x80]),
        Charset::Katakana => match b | 0x80 {
            c @ 0xA1..=0xDF => out.push(
                char::from_u32(0xFF61 + u32::from(c - 0xA1)).unwrap_or(char::REPLACEMENT_CHARACTER),
            ),
            _ => out.push(char::REPLACEMENT_CHARACTER),
        },
        Charset::Double(_) if !(0x21..=0x7E).contains(&(b & 0x7F)) => {
            out.push(char::REPLACEMENT_CHARACTER)
        }
        Charset::Double(encoding) => {
            let high = b & 0x80;
            match bytes.get(1) {
                Some(&second) if second & 0x80 == high && (second & 0x7F) > 0x20 && (second & 0x7F) < 0x7F => {
                    push_decoded(out, encoding, &[b | 0x80, second | 0x80]);
                    return 2;
                }
                _ => out.push(char::REPLACEMENT_CHARACTER),
            }
        }
        Charset::Unknown => out.push(char::REPLACEMENT_CHARACTER),
    }
    1
}

// Modified for NineSQL: the upstream decoder read unescaped text as UTF-8 (COMPOUND_TEXT starts
// in ISO 8859-1, so "é" from an input method arrived as the invalid UTF-8 byte 0xE9), only
// understood a whole-string UTF-8 segment or a JIS X 0208 prefix, returned errors for GB 2312 /
// KS C 5601, and panicked on a truncated UTF-8 segment (`left.len() - 3`). zed-xim `expect`ed the
// result, so the first non-ASCII preedit crashed the application. This decoder follows the
// COMPOUND_TEXT state machine (X Consortium "Compound Text Encoding", version 1.1.xf86.1):
// GL / GR designations, the `ESC % G` UTF-8 extension, extended segments and direction
// sequences. It never fails: malformed input and unknown character sets decode to U+FFFD.
/// Decodes COMPOUND_TEXT into UTF-8.
///
/// Never returns an error: bytes that cannot be decoded (unknown character sets, truncated or
/// malformed sequences) become U+FFFD. The `Result` is kept for API compatibility.
pub fn compound_text_to_utf8(bytes: &[u8]) -> Result<String, DecodeError> {
    let mut out = String::with_capacity(bytes.len());
    let mut gl = Charset::Ascii;
    let mut gr = Charset::Latin1;
    let mut i = 0;

    while i < bytes.len() {
        let b = bytes[i];
        match b {
            0x1B => {
                // ESC, intermediate bytes (0x20-0x2F), final byte (0x30-0x7E).
                let mut j = i + 1;
                while j < bytes.len() && (0x20..=0x2F).contains(&bytes[j]) {
                    j += 1;
                }
                let Some(&final_byte) = bytes.get(j) else {
                    // Truncated escape sequence at the end of the text.
                    break;
                };
                if !(0x30..=0x7E).contains(&final_byte) {
                    out.push(char::REPLACEMENT_CHARACTER);
                    i = j;
                    continue;
                }
                let intermediates = &bytes[i + 1..j];
                i = j + 1;
                match intermediates {
                    b"(" => {
                        gl = match final_byte {
                            // ASCII; JIS X 0201 roman differs from ASCII in two glyphs only.
                            b'B' | b'J' => Charset::Ascii,
                            b'I' => Charset::Katakana,
                            _ => Charset::Unknown,
                        }
                    }
                    b")" => {
                        gr = match final_byte {
                            b'I' => Charset::Katakana,
                            _ => Charset::Unknown,
                        }
                    }
                    b"-" => gr = iso8859(final_byte),
                    b"$(" => gl = double(final_byte),
                    b"$)" => gr = double(final_byte),
                    // ISO 2022 short form `ESC $ F` for the first three 94x94 sets.
                    b"$" => gl = double(final_byte),
                    b"%" if final_byte == b'G' => {
                        // UTF-8 segment up to `ESC % @` or the end of the text.
                        let rest = &bytes[i..];
                        let len = rest
                            .windows(UTF8_END.len())
                            .position(|w| w == UTF8_END)
                            .unwrap_or(rest.len());
                        out.push_str(&String::from_utf8_lossy(&rest[..len]));
                        i += (len + UTF8_END.len()).min(rest.len());
                    }
                    // `ESC % @` outside a UTF-8 segment: nothing to do.
                    b"%" if final_byte == b'@' => {}
                    b"%/" if (b'0'..=b'4').contains(&final_byte) => {
                        // Extended segment: two length bytes M L, then that many bytes.
                        match (bytes.get(i), bytes.get(i + 1)) {
                            (Some(&m), Some(&l)) if m >= 0x80 && l >= 0x80 => {
                                let len = usize::from(m - 0x80) * 128 + usize::from(l - 0x80);
                                out.push(char::REPLACEMENT_CHARACTER);
                                i = (i + 2).saturating_add(len).min(bytes.len());
                            }
                            _ => {
                                out.push(char::REPLACEMENT_CHARACTER);
                                i = bytes.len();
                            }
                        }
                    }
                    _ => {
                        // An unknown sequence: designate "unknown" to the side it names.
                        match intermediates.first() {
                            Some(b'(') | Some(b',') => gl = Charset::Unknown,
                            Some(b')') | Some(b'-') | Some(b'.') | Some(b'/') => {
                                gr = Charset::Unknown
                            }
                            Some(b'$') => match intermediates.get(1) {
                                Some(b'(') | Some(b',') => gl = Charset::Unknown,
                                _ => gr = Charset::Unknown,
                            },
                            _ => {}
                        }
                    }
                }
            }
            0x9B => {
                // CSI: direction sequences (`CSI 1 ]`, `CSI 2 ]`, `CSI ]`); skipped.
                let mut j = i + 1;
                while j < bytes.len() && (0x20..=0x3F).contains(&bytes[j]) {
                    j += 1;
                }
                i = (j + 1).min(bytes.len());
            }
            // C0 controls (only HT and NL are allowed) and space are the same in every set.
            0x00..=0x20 | 0x7F => {
                out.push(char::from(b));
                i += 1;
            }
            0x21..=0x7E => i += decode_char(&mut out, gl, &bytes[i..]),
            // C1 controls other than CSI are not allowed in COMPOUND_TEXT.
            0x80..=0x9F => {
                out.push(char::REPLACEMENT_CHARACTER);
                i += 1;
            }
            0xA0..=0xFF => i += decode_char(&mut out, gr, &bytes[i..]),
        }
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    #[test]
    fn korean() {
        const UTF8: &str = "가나다";
        const COMP: &[u8] = &[
            27, 37, 71, 234, 176, 128, 235, 130, 152, 235, 139, 164, 27, 37, 64,
        ];
        assert_eq!(crate::utf8_to_compound_text(UTF8), COMP);
        assert_eq!(crate::compound_text_to_utf8(COMP).unwrap(), UTF8);
    }

    #[test]
    fn iso_2011_jp() {
        const UTF8: &str = "東京";
        const COMP: &[u8] = &[27, 36, 40, 66, 69, 108, 53, 126];
        assert_eq!(crate::compound_text_to_utf8(COMP).unwrap(), UTF8);
    }

    // Modified for NineSQL: regression tests for the decoder rewrite.

    /// The input that crashed the application (ibus, m17n `latn-post`, typing "e'"): the
    /// preedit "é" arrives as unescaped ISO 8859-1.
    #[test]
    fn unescaped_text_is_latin1() {
        assert_eq!(crate::compound_text_to_utf8(&[0xE9]).unwrap(), "é");
        assert_eq!(crate::compound_text_to_utf8(b"caf\xe9 cr\xe8me").unwrap(), "café crème");
        // Explicit ISO 8859-1 designation, then back to ASCII.
        assert_eq!(
            crate::compound_text_to_utf8(b"\x1b-A\xe9\x1b(Bx").unwrap(),
            "éx"
        );
    }

    #[test]
    fn other_iso8859_parts() {
        // ISO 8859-2 "ą", ISO 8859-5 "Ж", ISO 8859-7 "Ω".
        assert_eq!(crate::compound_text_to_utf8(b"\x1b-B\xb1").unwrap(), "ą");
        assert_eq!(crate::compound_text_to_utf8(b"\x1b-L\xb6").unwrap(), "Ж");
        assert_eq!(crate::compound_text_to_utf8(b"\x1b-F\xd9").unwrap(), "Ω");
    }

    #[test]
    fn gb2312_and_ksc5601() {
        // "中文" in GL and GR, then ASCII.
        assert_eq!(
            crate::compound_text_to_utf8(b"\x1b$(AVPND\x1b(Bok").unwrap(),
            "中文ok"
        );
        assert_eq!(
            crate::compound_text_to_utf8(b"\x1b$)A\xd6\xd0\xce\xc4").unwrap(),
            "中文"
        );
        // "가" in GR (EUC-KR B0A1) and GL.
        assert_eq!(crate::compound_text_to_utf8(b"\x1b$)C\xb0\xa1").unwrap(), "가");
        assert_eq!(crate::compound_text_to_utf8(b"\x1b$(C0!").unwrap(), "가");
        // JIS X 0208 in GR.
        assert_eq!(
            crate::compound_text_to_utf8(b"\x1b$)B\xc5\xec\xb5\xfe").unwrap(),
            "東京"
        );
    }

    #[test]
    fn utf8_segments_anywhere() {
        assert_eq!(
            crate::compound_text_to_utf8(b"a\x1b%G\xc3\xa9\x1b%@b").unwrap(),
            "aéb"
        );
        // Unterminated segment, empty segment, truncated escapes: no panic.
        assert_eq!(crate::compound_text_to_utf8(b"\x1b%G\xc3\xa9").unwrap(), "é");
        assert_eq!(crate::compound_text_to_utf8(b"\x1b%G").unwrap(), "");
        assert_eq!(crate::compound_text_to_utf8(b"\x1b%").unwrap(), "");
        assert_eq!(crate::compound_text_to_utf8(b"\x1b").unwrap(), "");
        // Invalid UTF-8 inside a segment is replaced, not an error.
        assert_eq!(crate::compound_text_to_utf8(b"\x1b%G\xff\x1b%@").unwrap(), "\u{FFFD}");
    }

    #[test]
    fn unknown_sets_and_malformed_input_decode_to_replacement() {
        // Unknown 94x94 set (final byte 'D'): its characters are U+FFFD.
        assert_eq!(crate::compound_text_to_utf8(b"\x1b$(D!!x").unwrap(), "\u{FFFD}\u{FFFD}\u{FFFD}");
        // Half of a double-byte character at the end.
        assert_eq!(crate::compound_text_to_utf8(b"\x1b$)A\xd6").unwrap(), "\u{FFFD}");
        // Direction sequences are skipped; C1 controls are replaced.
        assert_eq!(crate::compound_text_to_utf8(b"\x9b2]ab\x9b]\x85").unwrap(), "ab\u{FFFD}");
        // Extended segment: one U+FFFD, then the text after it.
        assert_eq!(
            crate::compound_text_to_utf8(b"\x1b%/1\x80\x84abcdz").unwrap(),
            "\u{FFFD}z"
        );
    }

    #[test]
    fn decoding_never_fails_or_panics() {
        let mut buf = [0u8; 3];
        for a in 0..=255u8 {
            buf[0] = a;
            assert!(crate::compound_text_to_utf8(&buf[..1]).is_ok());
            for b in 0..=255u8 {
                buf[1] = b;
                assert!(crate::compound_text_to_utf8(&buf[..2]).is_ok());
            }
        }
        // Pseudo-random longer inputs, biased towards ESC and its intermediates.
        let alphabet = [0x1B, 0x24, 0x25, 0x28, 0x29, 0x2D, 0x2F, 0x40, 0x41, 0x42, 0x43, 0x47];
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        for _ in 0..20_000 {
            let mut input = alloc::vec::Vec::new();
            for _ in 0..(state % 24) {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                let r = (state >> 24) as u8;
                input.push(if r & 1 == 0 { alphabet[usize::from(r >> 1) % alphabet.len()] } else { r });
            }
            state = state.wrapping_add(1);
            assert!(crate::compound_text_to_utf8(&input).is_ok());
        }
    }
}
