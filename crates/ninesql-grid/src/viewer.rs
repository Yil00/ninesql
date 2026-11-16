//! Value viewers of the selected cell (F-DATA-05): text, JSON (pretty, read-only), XML (pretty
//! text), binary (hex dump) and image preview (PNG and JPEG; GIF and animated PNG are shown as
//! hex, see [`sniff_image`]).
//!
//! Everything here reads the value the grid already holds, never more (I6): a value the driver
//! cut (`ninesql.truncated`) is shown as its kept prefix and flagged
//! ([`CellInspection::cut_by_driver`], I8), and each viewer has its own size bound, flagged
//! too. Pretty printers only re-indent: they never evaluate entities, fetch DTDs or resolve
//! anything (an XML viewer that resolved external entities would reach the network, I2).
//!
//! Images are decoded by GPUI only after [`sniff_image`] has read the dimensions from the file
//! header and checked them against [`MAX_IMAGE_SIDE`] / [`MAX_IMAGE_PIXELS`] and the byte size
//! against [`MAX_IMAGE_BYTES`], so a small value cannot expand into a huge bitmap. Only formats
//! decoded as a single frame are passed to GPUI.

use std::sync::Arc;

use ninesql_core::transfer::value::{CellValue, cell};

use crate::format::{ValueKind, is_bidi_control, write_copy};
use crate::model::ResultSetModel;

/// Text shown at most by the text, JSON and XML viewers (input bytes).
pub const MAX_VIEWER_TEXT_BYTES: usize = 1024 * 1024;
/// Output of a pretty printer at most (indentation can grow the text).
pub const MAX_PRETTY_BYTES: usize = 4 * 1024 * 1024;
/// Deepest indentation level a pretty printer writes (deeper levels stay at this indent).
pub const MAX_INDENT_DEPTH: usize = 32;
/// Bytes shown at most by the hex dump.
pub const MAX_HEX_BYTES: usize = 64 * 1024;
/// Largest value previewed as an image.
pub const MAX_IMAGE_BYTES: usize = 16 * 1024 * 1024;
/// Widest or tallest image previewed, in pixels.
pub const MAX_IMAGE_SIDE: u32 = 8192;
/// Most pixels of a previewed image.
pub const MAX_IMAGE_PIXELS: u64 = 4096 * 4096;

/// A viewer of the value panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ViewerKind {
    /// Plain text.
    Text,
    /// Pretty-printed JSON (read-only).
    Json,
    /// Pretty-printed XML text (read-only).
    Xml,
    /// Hexadecimal dump.
    Hex,
    /// Image preview.
    Image,
}

impl ViewerKind {
    /// Tab label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Text => "Text",
            Self::Json => "JSON",
            Self::Xml => "XML",
            Self::Hex => "Hex",
            Self::Image => "Image",
        }
    }
}

/// An image format the preview decodes. GIF and animated PNG are not among them (see
/// [`sniff_image`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageKind {
    /// PNG (a single image).
    Png,
    /// JPEG.
    Jpeg,
}

/// What the header of an image says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageInfo {
    /// Format.
    pub kind: ImageKind,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

/// Why a value is not previewed as an image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageRefusal {
    /// Not a PNG or JPEG (or a damaged header).
    NotAnImage,
    /// A GIF: GPUI decodes every GIF frame with no limit, so GIF is not previewed (the hex
    /// viewer shows it).
    GifNotSupported,
    /// An animated PNG: refused like GIF.
    AnimatedNotSupported,
    /// Over [`MAX_IMAGE_BYTES`].
    TooLarge,
    /// Over [`MAX_IMAGE_SIDE`] or [`MAX_IMAGE_PIXELS`].
    TooManyPixels {
        /// Width.
        width: u32,
        /// Height.
        height: u32,
    },
    /// The driver cut the value: a partial image is not decoded.
    Cut,
}

impl ImageRefusal {
    /// User-facing text.
    #[must_use]
    pub fn text(&self) -> String {
        match self {
            Self::NotAnImage => "Not a PNG or JPEG image: image preview is not supported for this value.".into(),
            Self::GifNotSupported => "GIF preview not supported; showing hex.".into(),
            Self::AnimatedNotSupported => "Animated PNG preview not supported; showing hex.".into(),
            Self::TooLarge => format!(
                "The image is larger than {} MiB: not previewed.",
                MAX_IMAGE_BYTES / (1024 * 1024)
            ),
            Self::TooManyPixels { width, height } => format!(
                "The image is {width} × {height} pixels, over the preview limit: not previewed."
            ),
            Self::Cut => "The value was cut by the driver at the value size limit: a partial image is not previewed.".into(),
        }
    }
}

/// The selected cell, read for the value panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellInspection {
    /// Column name.
    pub column: String,
    /// Type shown (engine type, else Arrow type).
    pub type_name: String,
    /// How the grid renders the column.
    pub kind: ValueKind,
    /// The value is NULL.
    pub is_null: bool,
    /// The driver cut values of this column: what is shown is a prefix (I8).
    pub cut_by_driver: bool,
    /// Text form (text values: at most [`MAX_VIEWER_TEXT_BYTES`]; other non-binary values:
    /// their copy text).
    pub text: String,
    /// The text was cut by the viewer's bound.
    pub text_capped: bool,
    /// Binary value (at most [`MAX_IMAGE_BYTES`], kept only for binary columns), shared so
    /// that cloning an inspection never copies it.
    pub bytes: Option<Arc<[u8]>>,
    /// Size of the binary value as held by the grid.
    pub byte_len: usize,
    /// The viewers offered, the default first.
    pub viewers: Vec<ViewerKind>,
}

/// Reads model row `row`, result column `col` for the value panel. `None` outside the loaded
/// rows.
#[must_use]
pub fn inspect_cell(model: &ResultSetModel, row: usize, col: usize) -> Option<CellInspection> {
    let meta = model.columns().get(col)?;
    let (array, r) = model.array_at(row, col)?;
    let value = cell(array, r);
    let mut out = CellInspection {
        column: meta.name.clone(),
        type_name: meta.type_name().to_owned(),
        kind: meta.kind,
        is_null: matches!(value, CellValue::Null),
        cut_by_driver: meta.truncated_values,
        text: String::new(),
        text_capped: false,
        bytes: None,
        byte_len: 0,
        viewers: Vec::new(),
    };
    if out.is_null {
        out.viewers.push(ViewerKind::Text);
        return Some(out);
    }
    match value {
        CellValue::Bytes(b) => {
            out.byte_len = b.len();
            let keep = b.len().min(MAX_IMAGE_BYTES);
            out.bytes = Some(Arc::from(&b[..keep]));
            out.viewers.push(ViewerKind::Hex);
            if sniff_image(b).is_ok() {
                out.viewers.insert(0, ViewerKind::Image);
            }
        }
        CellValue::Text(s) => {
            let (t, capped) = cap_str(s, MAX_VIEWER_TEXT_BYTES);
            out.text = t.to_owned();
            out.text_capped = capped;
            let engine_xml = meta
                .engine_type
                .as_deref()
                .is_some_and(|t| t.eq_ignore_ascii_case("xml"));
            if meta.kind == ValueKind::Json {
                out.viewers = vec![ViewerKind::Json, ViewerKind::Text];
            } else if engine_xml {
                out.viewers = vec![ViewerKind::Xml, ViewerKind::Text];
            } else {
                out.viewers.push(ViewerKind::Text);
                if looks_like_json(&out.text) {
                    out.viewers.push(ViewerKind::Json);
                }
                if looks_like_xml(&out.text) {
                    out.viewers.push(ViewerKind::Xml);
                }
            }
        }
        _ => {
            write_copy(&mut out.text, array, r, "");
            out.viewers.push(ViewerKind::Text);
        }
    }
    Some(out)
}

/// The longest prefix of `s` of at most `max` bytes on a character boundary.
fn cap_str(s: &str, max: usize) -> (&str, bool) {
    if s.len() <= max {
        return (s, false);
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    (&s[..end], true)
}

fn looks_like_json(s: &str) -> bool {
    let t = s.trim();
    ((t.starts_with('{') && t.ends_with('}')) || (t.starts_with('[') && t.ends_with(']')))
        && pretty_json(t).is_ok()
}

fn looks_like_xml(s: &str) -> bool {
    let t = s.trim();
    t.starts_with('<') && t.ends_with('>')
}

/// Text for a multi-line viewer: line breaks and tabs kept, bidirectional controls as
/// `<U+XXXX>`, other control characters as U+FFFD.
#[must_use]
pub fn viewer_text(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for c in raw.chars() {
        match c {
            '\n' | '\t' => out.push(c),
            '\r' => {}
            c if is_bidi_control(c) => out.push_str(&format!("<U+{:04X}>", u32::from(c))),
            c if c.is_control() => out.push('\u{FFFD}'),
            c => out.push(c),
        }
    }
    out
}

/// Why a text is not shown as JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JsonError {
    /// Byte offset of the problem.
    pub offset: usize,
}

fn indent(out: &mut String, depth: usize, width: usize) {
    out.push('\n');
    for _ in 0..depth.min(MAX_INDENT_DEPTH) * width {
        out.push(' ');
    }
}

/// Re-indents JSON text (two spaces per level). Strings are copied byte for byte; the structure
/// is checked (balanced brackets, terminated strings, only JSON punctuation and literals outside
/// strings). Output past [`MAX_PRETTY_BYTES`] stops with a `…` line.
///
/// # Errors
/// [`JsonError`] at the first structural problem.
pub fn pretty_json(text: &str) -> Result<String, JsonError> {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len() + text.len() / 4);
    let mut stack: Vec<u8> = Vec::new();
    let mut i = 0;
    let mut empty_container = false;
    while i < bytes.len() {
        if out.len() > MAX_PRETTY_BYTES {
            out.push_str("\n…");
            return Ok(out);
        }
        let b = bytes[i];
        match b {
            b' ' | b'\t' | b'\n' | b'\r' => {
                i += 1;
                continue;
            }
            b'"' => {
                let start = i;
                i += 1;
                loop {
                    match bytes.get(i) {
                        None => return Err(JsonError { offset: start }),
                        Some(b'\\') => i += 2,
                        Some(b'"') => {
                            i += 1;
                            break;
                        }
                        Some(_) => i += 1,
                    }
                }
                if empty_container {
                    indent(&mut out, stack.len(), 2);
                    empty_container = false;
                }
                out.push_str(text.get(start..i).ok_or(JsonError { offset: start })?);
                continue;
            }
            b'{' | b'[' => {
                if empty_container {
                    indent(&mut out, stack.len(), 2);
                }
                stack.push(if b == b'{' { b'}' } else { b']' });
                out.push(b as char);
                empty_container = true;
            }
            b'}' | b']' => {
                if stack.pop() != Some(b) {
                    return Err(JsonError { offset: i });
                }
                if !empty_container {
                    indent(&mut out, stack.len(), 2);
                }
                empty_container = false;
                out.push(b as char);
            }
            b',' => {
                if stack.is_empty() || empty_container {
                    return Err(JsonError { offset: i });
                }
                out.push(',');
                indent(&mut out, stack.len(), 2);
            }
            b':' => {
                if stack.last() != Some(&b'}') {
                    return Err(JsonError { offset: i });
                }
                out.push_str(": ");
            }
            c if c.is_ascii_alphanumeric() || matches!(c, b'-' | b'+' | b'.') => {
                let start = i;
                while i < bytes.len()
                    && (bytes[i].is_ascii_alphanumeric() || matches!(bytes[i], b'-' | b'+' | b'.'))
                {
                    i += 1;
                }
                if empty_container {
                    indent(&mut out, stack.len(), 2);
                    empty_container = false;
                }
                out.push_str(&text[start..i]);
                continue;
            }
            _ => return Err(JsonError { offset: i }),
        }
        i += 1;
    }
    if !stack.is_empty() {
        return Err(JsonError {
            offset: bytes.len(),
        });
    }
    Ok(out)
}

/// Re-indents XML text (two spaces per level): one tag or text run per line. Comments,
/// processing instructions, `<!DOCTYPE …>` and CDATA sections are copied as they are; entities
/// are never expanded and nothing is resolved. Malformed input is indented as well as possible
/// (the viewer is read-only). Output past [`MAX_PRETTY_BYTES`] stops with a `…` line.
#[must_use]
pub fn pretty_xml(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + text.len() / 4);
    let mut depth = 0usize;
    let mut rest = text;
    let mut first = true;
    let mut line = |out: &mut String, depth: usize, s: &str| {
        if !first {
            out.push('\n');
        }
        first = false;
        for _ in 0..depth.min(MAX_INDENT_DEPTH) * 2 {
            out.push(' ');
        }
        out.push_str(s);
    };
    while !rest.is_empty() {
        if out.len() > MAX_PRETTY_BYTES {
            out.push_str("\n…");
            break;
        }
        if let Some(after) = rest.strip_prefix('<') {
            let (end, kind) = if after.starts_with("!--") {
                (rest.find("-->").map(|p| p + 3), 0)
            } else if after.starts_with("![CDATA[") {
                (rest.find("]]>").map(|p| p + 3), 0)
            } else if after.starts_with('?') {
                (rest.find("?>").map(|p| p + 2), 0)
            } else {
                // A tag: `>` inside a quoted attribute value does not end it.
                let mut quote: Option<char> = None;
                let mut end = None;
                for (i, c) in rest.char_indices().skip(1) {
                    match (quote, c) {
                        (Some(q), c) if c == q => quote = None,
                        (Some(_), _) => {}
                        (None, '"' | '\'') => quote = Some(c),
                        (None, '>') => {
                            end = Some(i + 1);
                            break;
                        }
                        _ => {}
                    }
                }
                let kind = if after.starts_with('/') {
                    -1
                } else if after.starts_with('!')
                    || rest[..end.unwrap_or(rest.len())].ends_with("/>")
                {
                    0
                } else {
                    1
                };
                (end, kind)
            };
            let end = end.unwrap_or(rest.len());
            let token = &rest[..end];
            if kind < 0 {
                depth = depth.saturating_sub(1);
            }
            line(&mut out, depth, token);
            if kind > 0 {
                depth += 1;
            }
            rest = &rest[end..];
        } else {
            let end = rest.find('<').unwrap_or(rest.len());
            let t = rest[..end].trim();
            if !t.is_empty() {
                line(&mut out, depth, t);
            }
            rest = &rest[end..];
        }
    }
    out
}

/// A classic hex dump of at most [`MAX_HEX_BYTES`] bytes: offset, 16 bytes in hex, and their
/// printable ASCII. Returns the text and whether the bound cut it.
#[must_use]
pub fn hex_dump(bytes: &[u8]) -> (String, bool) {
    let capped = bytes.len() > MAX_HEX_BYTES;
    let shown = &bytes[..bytes.len().min(MAX_HEX_BYTES)];
    let mut out = String::with_capacity(shown.len() * 4 + 16);
    for (n, chunk) in shown.chunks(16).enumerate() {
        if n > 0 {
            out.push('\n');
        }
        out.push_str(&format!("{:08x}  ", n * 16));
        for i in 0..16 {
            match chunk.get(i) {
                Some(b) => out.push_str(&format!("{b:02x} ")),
                None => out.push_str("   "),
            }
            if i == 7 {
                out.push(' ');
            }
        }
        out.push_str(" |");
        for &b in chunk {
            out.push(if b.is_ascii_graphic() || b == b' ' {
                b as char
            } else {
                '.'
            });
        }
        out.push('|');
    }
    (out, capped)
}

/// Reads the format and size of a PNG or JPEG from its header, and checks the preview bounds.
/// Nothing is decoded.
///
/// GIF is refused ([`ImageRefusal::GifNotSupported`]): GPUI decodes every frame of a GIF with no
/// limit, so a small file with thousands of frames, or a frame rectangle larger than the logical
/// screen, would expand without bound (I6). An animated PNG (an `acTL`, `fcTL` or `fdAT` chunk)
/// is refused for the same reason, and so is a PNG whose chunks do not walk cleanly up to
/// `IEND`. A JPEG is accepted only when exactly one start-of-frame lies before the first
/// start-of-scan, so the size checked here is the size the decoder allocates.
///
/// # Errors
/// [`ImageRefusal`].
pub fn sniff_image(bytes: &[u8]) -> Result<ImageInfo, ImageRefusal> {
    let info = if bytes.starts_with(PNG_SIGNATURE) {
        png_info(bytes)?
    } else if is_gif(bytes) {
        return Err(ImageRefusal::GifNotSupported);
    } else if bytes.starts_with(&[0xFF, 0xD8]) {
        jpeg_size(bytes).ok_or(ImageRefusal::NotAnImage)?
    } else {
        return Err(ImageRefusal::NotAnImage);
    };
    if info.width == 0 || info.height == 0 {
        return Err(ImageRefusal::NotAnImage);
    }
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(ImageRefusal::TooLarge);
    }
    if info.width > MAX_IMAGE_SIDE
        || info.height > MAX_IMAGE_SIDE
        || u64::from(info.width) * u64::from(info.height) > MAX_IMAGE_PIXELS
    {
        return Err(ImageRefusal::TooManyPixels {
            width: info.width,
            height: info.height,
        });
    }
    Ok(info)
}

const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

/// Whether `bytes` start with a GIF signature.
#[must_use]
pub fn is_gif(bytes: &[u8]) -> bool {
    bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a")
}

/// Walks the PNG chunks: `IHDR` first, then every chunk up to `IEND` (or the end of the value),
/// refusing the animation chunks wherever they are and any chunk that runs past the end. Chunk CRCs are not checked (the decoder
/// does), only the framing.
fn png_info(b: &[u8]) -> Result<ImageInfo, ImageRefusal> {
    let be = |i: usize| -> Option<u32> {
        b.get(i..i + 4)
            .map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
    };
    let mut i = PNG_SIGNATURE.len();
    let mut info = None;
    while i < b.len() {
        let len = be(i).ok_or(ImageRefusal::NotAnImage)? as usize;
        let kind = b.get(i + 4..i + 8).ok_or(ImageRefusal::NotAnImage)?;
        let data = i + 8;
        let end = data
            .checked_add(len)
            .and_then(|e| e.checked_add(4))
            .ok_or(ImageRefusal::NotAnImage)?;
        match kind {
            b"IHDR" => {
                if info.is_some() || len != 13 || i != PNG_SIGNATURE.len() {
                    return Err(ImageRefusal::NotAnImage);
                }
                info = Some(ImageInfo {
                    kind: ImageKind::Png,
                    width: be(data).ok_or(ImageRefusal::NotAnImage)?,
                    height: be(data + 4).ok_or(ImageRefusal::NotAnImage)?,
                });
            }
            _ if info.is_none() => return Err(ImageRefusal::NotAnImage),
            b"acTL" | b"fcTL" | b"fdAT" => return Err(ImageRefusal::AnimatedNotSupported),
            b"IEND" => break,
            _ => {}
        }
        if end > b.len() {
            // A chunk running past the end of the value: the framing is broken, refused.
            return Err(ImageRefusal::NotAnImage);
        }
        i = end;
    }
    info.ok_or(ImageRefusal::NotAnImage)
}

/// Walks the JPEG markers up to the first start-of-scan; exactly one start-of-frame must come
/// before it.
fn jpeg_size(b: &[u8]) -> Option<ImageInfo> {
    let mut i = 2;
    let mut found = None;
    while i + 4 <= b.len() {
        if b[i] != 0xFF {
            return None;
        }
        let marker = b[i + 1];
        if marker == 0xFF {
            i += 1;
            continue;
        }
        if matches!(marker, 0x01 | 0xD0..=0xD7) {
            i += 2;
            continue;
        }
        if matches!(marker, 0xD8 | 0xD9) {
            // A second SOI or an EOI before any scan: not a picture this walk trusts.
            return None;
        }
        let len = usize::from(u16::from_be_bytes([b[i + 2], b[i + 3]]));
        if len < 2 {
            return None;
        }
        if marker == 0xDA {
            return found;
        }
        let sof = matches!(marker, 0xC0..=0xCF) && !matches!(marker, 0xC4 | 0xC8 | 0xCC);
        if sof {
            if found.is_some() || i + 9 > b.len() {
                return None;
            }
            let height = u32::from(u16::from_be_bytes([b[i + 5], b[i + 6]]));
            let width = u32::from(u16::from_be_bytes([b[i + 7], b[i + 8]]));
            found = Some(ImageInfo {
                kind: ImageKind::Jpeg,
                width,
                height,
            });
        }
        i += 2 + len;
    }
    None
}

/// What a viewer shows for a text-like viewer kind: the text and the notes under it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewerOutput {
    /// Text to show (multi-line, read-only).
    pub text: String,
    /// Notes: cut by the driver, capped by the viewer, not valid JSON…
    pub notes: Vec<String>,
}

/// Renders `inspection` with a text-like viewer (`Text`, `Json`, `Xml`, `Hex`); `Image` returns
/// the refusal or the image size as a note (the panel draws the image itself).
#[must_use]
pub fn render(inspection: &CellInspection, kind: ViewerKind) -> ViewerOutput {
    let mut notes = Vec::new();
    if inspection.cut_by_driver {
        notes.push(
            "Values of this column were cut by the driver at the value size limit: this may be a prefix of the value.".to_owned(),
        );
    }
    if inspection.is_null {
        return ViewerOutput {
            text: "NULL".to_owned(),
            notes,
        };
    }
    if inspection.text_capped {
        notes.push(format!(
            "Showing the first {} KiB of the value.",
            MAX_VIEWER_TEXT_BYTES / 1024
        ));
    }
    let text = match kind {
        ViewerKind::Text => viewer_text(&inspection.text),
        ViewerKind::Json => match pretty_json(inspection.text.trim()) {
            Ok(t) => viewer_text(&t),
            Err(e) => {
                notes.push(format!(
                    "Not valid JSON (at byte {}): shown as text.",
                    e.offset
                ));
                viewer_text(&inspection.text)
            }
        },
        ViewerKind::Xml => viewer_text(&pretty_xml(inspection.text.trim())),
        ViewerKind::Hex => {
            let bytes = inspection
                .bytes
                .as_deref()
                .unwrap_or(inspection.text.as_bytes());
            let (t, capped) = hex_dump(bytes);
            if let Err(r @ (ImageRefusal::GifNotSupported | ImageRefusal::AnimatedNotSupported)) =
                sniff_image(bytes)
            {
                notes.push(r.text());
            }
            if capped {
                notes.push(format!(
                    "Showing the first {} KiB of {} bytes.",
                    MAX_HEX_BYTES / 1024,
                    inspection.byte_len.max(bytes.len())
                ));
            }
            t
        }
        ViewerKind::Image => {
            let bytes = inspection.bytes.as_deref().unwrap_or_default();
            match image_for_preview(inspection) {
                Ok(info) => {
                    notes.push(format!(
                        "{:?} image, {} × {} pixels, {} bytes.",
                        info.kind,
                        info.width,
                        info.height,
                        bytes.len()
                    ));
                }
                Err(r) => notes.push(r.text()),
            }
            String::new()
        }
    };
    ViewerOutput { text, notes }
}

/// Whether the inspected value may be decoded for the preview.
///
/// # Errors
/// [`ImageRefusal`], including [`ImageRefusal::Cut`] for a value the driver cut.
pub fn image_for_preview(inspection: &CellInspection) -> Result<ImageInfo, ImageRefusal> {
    if inspection.cut_by_driver {
        return Err(ImageRefusal::Cut);
    }
    if inspection.byte_len > MAX_IMAGE_BYTES {
        return Err(ImageRefusal::TooLarge);
    }
    sniff_image(inspection.bytes.as_deref().unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use ninesql_core::driver_api::arrow_array::{BinaryArray, RecordBatch, StringArray};
    use ninesql_core::driver_api::arrow_schema::{DataType, Field, Schema};
    use ninesql_core::driver_api::{ENGINE_TYPE_METADATA_KEY, TRUNCATED_VALUES_METADATA_KEY};

    use super::*;

    fn chunk(v: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        v.extend_from_slice(&u32::try_from(data.len()).unwrap().to_be_bytes());
        v.extend_from_slice(kind);
        v.extend_from_slice(data);
        v.extend_from_slice(&[0, 0, 0, 0]); // CRC: not checked by the sniffer.
    }

    fn ihdr(width: u32, height: u32) -> Vec<u8> {
        let mut d = width.to_be_bytes().to_vec();
        d.extend_from_slice(&height.to_be_bytes());
        d.extend_from_slice(&[8, 6, 0, 0, 0]);
        d
    }

    /// A PNG framing with `extra` chunks between `IHDR` and `IDAT`.
    fn png_with(width: u32, height: u32, extra: &[(&[u8; 4], &[u8])]) -> Vec<u8> {
        let mut v = PNG_SIGNATURE.to_vec();
        chunk(&mut v, b"IHDR", &ihdr(width, height));
        for (k, d) in extra {
            chunk(&mut v, k, d);
        }
        chunk(&mut v, b"IDAT", &[0; 8]);
        chunk(&mut v, b"IEND", &[]);
        v
    }

    fn png(width: u32, height: u32) -> Vec<u8> {
        png_with(width, height, &[])
    }

    /// SOI, an APP0 segment, `sofs` start-of-frame segments, then SOS.
    fn jpeg(sofs: &[(u8, u16, u16)], sos: bool) -> Vec<u8> {
        let mut v = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00];
        for &(marker, h, w) in sofs {
            v.extend_from_slice(&[0xFF, marker, 0x00, 0x0B, 0x08]);
            v.extend_from_slice(&h.to_be_bytes());
            v.extend_from_slice(&w.to_be_bytes());
            v.extend_from_slice(&[0x01, 0x01, 0x11, 0x00]);
        }
        if sos {
            v.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x3F, 0x00]);
            v.extend_from_slice(&[0x12, 0x34, 0xFF, 0xD9]);
        }
        v
    }

    #[test]
    fn json_is_reindented_and_strings_kept() {
        let out = pretty_json(r#"{"a":[1,2,{"b":"x,y:{"}],"c":{},"d":[]}"#).unwrap();
        assert_eq!(
            out,
            "{\n  \"a\": [\n    1,\n    2,\n    {\n      \"b\": \"x,y:{\"\n    }\n  ],\n  \"c\": {},\n  \"d\": []\n}"
        );
        assert!(pretty_json(r#"{"a":1"#).is_err());
        assert!(pretty_json(r#"{"a":1]"#).is_err());
        assert!(pretty_json(r#"{"a" = 1}"#).is_err());
        assert!(pretty_json(r#""unterminated"#).is_err());
        assert_eq!(pretty_json(r#""s\"q""#).unwrap(), r#""s\"q""#);
    }

    #[test]
    fn deep_json_indent_is_bounded() {
        let deep = format!("{}{}", "[".repeat(10_000), "]".repeat(10_000));
        let out = pretty_json(&deep).unwrap();
        assert!(out.len() <= 20_000 * (MAX_INDENT_DEPTH * 2 + 2));
    }

    #[test]
    fn xml_is_reindented_without_resolving_anything() {
        let out = pretty_xml(
            r#"<?xml version="1.0"?><!DOCTYPE a SYSTEM "http://example.invalid/a.dtd"><a x="1>2"><b>t &amp; u</b><c/><!-- n --></a>"#,
        );
        assert_eq!(
            out,
            "<?xml version=\"1.0\"?>\n<!DOCTYPE a SYSTEM \"http://example.invalid/a.dtd\">\n<a x=\"1>2\">\n  <b>\n    t &amp; u\n  </b>\n  <c/>\n  <!-- n -->\n</a>"
        );
        // Malformed input never panics.
        let _ = pretty_xml("</a></b><c");
        let _ = pretty_xml("<a x=\"unterminated>");
    }

    #[test]
    fn hex_dump_is_bounded() {
        let (t, capped) = hex_dump(b"Hello, world!\x00\x01\xff");
        assert_eq!(
            t,
            "00000000  48 65 6c 6c 6f 2c 20 77  6f 72 6c 64 21 00 01 ff  |Hello, world!...|"
        );
        assert!(!capped);
        let big = vec![0u8; MAX_HEX_BYTES + 1];
        let (t, capped) = hex_dump(&big);
        assert!(capped);
        assert_eq!(t.lines().count(), MAX_HEX_BYTES / 16);
    }

    #[test]
    fn images_are_sniffed_and_bounded() {
        assert_eq!(
            sniff_image(&png(10, 20)),
            Ok(ImageInfo {
                kind: ImageKind::Png,
                width: 10,
                height: 20
            })
        );
        assert_eq!(
            sniff_image(&png(100_000, 100_000)),
            Err(ImageRefusal::TooManyPixels {
                width: 100_000,
                height: 100_000
            })
        );
        assert_eq!(
            sniff_image(&png(5000, 5000)),
            Err(ImageRefusal::TooManyPixels {
                width: 5000,
                height: 5000
            })
        );
        assert_eq!(
            sniff_image(&jpeg(&[(0xC0, 7, 9)], true)),
            Ok(ImageInfo {
                kind: ImageKind::Jpeg,
                width: 9,
                height: 7
            })
        );
        assert_eq!(sniff_image(b"BM...."), Err(ImageRefusal::NotAnImage));
        assert_eq!(
            sniff_image(&[0xFF, 0xD8, 0xFF]),
            Err(ImageRefusal::NotAnImage)
        );
        assert_eq!(sniff_image(&png(0, 4)), Err(ImageRefusal::NotAnImage));
    }

    #[test]
    fn gif_is_never_decoded() {
        // A tiny logical screen whose single frame descriptor claims 65535 × 65535: GPUI would
        // decode it with no limit, so every GIF is refused and shown as hex.
        let mut bomb = b"GIF89a\x02\x00\x02\x00\x00\x00\x00".to_vec();
        bomb.extend_from_slice(&[0x2C, 0, 0, 0, 0, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]);
        bomb.extend_from_slice(&[0x02, 0x02, 0x44, 0x01, 0x00, 0x3B]);
        assert_eq!(sniff_image(&bomb), Err(ImageRefusal::GifNotSupported));
        // Thousands of 1 × 1 frames: refused as well.
        let mut many = b"GIF87a\x01\x00\x01\x00\x00\x00\x00".to_vec();
        for _ in 0..10_000 {
            many.extend_from_slice(&[0x2C, 0, 0, 0, 0, 1, 0, 1, 0, 0, 0x02, 0x02, 0x44, 0x01, 0]);
        }
        many.push(0x3B);
        assert_eq!(sniff_image(&many), Err(ImageRefusal::GifNotSupported));
        assert_eq!(
            ImageRefusal::GifNotSupported.text(),
            "GIF preview not supported; showing hex."
        );
        // The hex viewer says why.
        let m = model(
            vec![Field::new("b", DataType::Binary, true)],
            vec![Arc::new(BinaryArray::from(vec![Some(bomb.as_slice())]))],
        );
        let i = inspect_cell(&m, 0, 0).unwrap();
        assert_eq!(i.viewers, vec![ViewerKind::Hex]);
        assert_eq!(image_for_preview(&i), Err(ImageRefusal::GifNotSupported));
        let out = render(&i, ViewerKind::Hex);
        assert!(
            out.notes
                .iter()
                .any(|n| n.contains("GIF preview not supported")),
            "{:?}",
            out.notes
        );
    }

    #[test]
    fn animated_and_malformed_png_are_refused() {
        let actl: &[u8] = &[0, 0, 0, 200, 0, 0, 0, 0];
        assert_eq!(
            sniff_image(&png_with(2, 2, &[(b"acTL", actl)])),
            Err(ImageRefusal::AnimatedNotSupported)
        );
        // An animation chunk anywhere, even after the image data.
        let mut late = png(2, 2);
        late.truncate(late.len() - 12); // drop IEND
        chunk(&mut late, b"fcTL", &[0; 26]);
        chunk(&mut late, b"IEND", &[]);
        assert_eq!(sniff_image(&late), Err(ImageRefusal::AnimatedNotSupported));
        let mut fdat = png(2, 2);
        fdat.truncate(fdat.len() - 12);
        chunk(&mut fdat, b"fdAT", &[0; 8]);
        assert_eq!(sniff_image(&fdat), Err(ImageRefusal::AnimatedNotSupported));
        // A chunk running past the end of the value.
        let mut overrun = png(2, 2);
        overrun.truncate(overrun.len() - 12);
        overrun.extend_from_slice(&[0x7F, 0xFF, 0xFF, 0xFF]);
        overrun.extend_from_slice(b"tEXt");
        assert_eq!(sniff_image(&overrun), Err(ImageRefusal::NotAnImage));
        // IHDR not first, IHDR twice, IHDR of the wrong length, a bare signature.
        let mut v = PNG_SIGNATURE.to_vec();
        chunk(&mut v, b"tEXt", b"k\0v");
        chunk(&mut v, b"IHDR", &ihdr(2, 2));
        assert_eq!(sniff_image(&v), Err(ImageRefusal::NotAnImage));
        assert_eq!(
            sniff_image(&png_with(2, 2, &[(b"IHDR", &ihdr(9000, 9000))])),
            Err(ImageRefusal::NotAnImage)
        );
        let mut short = PNG_SIGNATURE.to_vec();
        chunk(&mut short, b"IHDR", &[0, 0, 0, 2, 0, 0, 0, 2]);
        assert_eq!(sniff_image(&short), Err(ImageRefusal::NotAnImage));
        assert_eq!(sniff_image(PNG_SIGNATURE), Err(ImageRefusal::NotAnImage));
        // Other formats GPUI could decode are never claimed (WebP, BMP, TIFF, ICO).
        assert_eq!(
            sniff_image(b"RIFF\x10\x00\x00\x00WEBPVP8X"),
            Err(ImageRefusal::NotAnImage)
        );
        assert_eq!(
            sniff_image(b"II*\x00\x08\x00"),
            Err(ImageRefusal::NotAnImage)
        );
        assert_eq!(
            sniff_image(b"\x00\x00\x01\x00"),
            Err(ImageRefusal::NotAnImage)
        );
    }

    #[test]
    fn jpeg_size_comes_from_the_only_frame_before_the_scan() {
        // A small first frame then a huge second one: refused, whichever the decoder reads.
        assert_eq!(
            sniff_image(&jpeg(&[(0xC0, 7, 9), (0xC2, 65_535, 65_535)], true)),
            Err(ImageRefusal::NotAnImage)
        );
        // No start-of-scan: not a complete header.
        assert_eq!(
            sniff_image(&jpeg(&[(0xC0, 7, 9)], false)),
            Err(ImageRefusal::NotAnImage)
        );
        // No frame before the scan.
        assert_eq!(sniff_image(&jpeg(&[], true)), Err(ImageRefusal::NotAnImage));
        // Height 0 (defined later by DNL) is refused.
        assert_eq!(
            sniff_image(&jpeg(&[(0xC0, 0, 9)], true)),
            Err(ImageRefusal::NotAnImage)
        );
        // Over the bounds.
        assert_eq!(
            sniff_image(&jpeg(&[(0xC1, 65_535, 65_535)], true)),
            Err(ImageRefusal::TooManyPixels {
                width: 65_535,
                height: 65_535
            })
        );
        // EOI or a second SOI before the scan.
        let mut eoi = jpeg(&[], false);
        eoi.extend_from_slice(&[0xFF, 0xD9]);
        eoi.extend_from_slice(&jpeg(&[(0xC0, 7, 9)], true)[2..]);
        assert_eq!(sniff_image(&eoi), Err(ImageRefusal::NotAnImage));
    }

    fn model(
        fields: Vec<Field>,
        arrays: Vec<ninesql_core::driver_api::arrow_array::ArrayRef>,
    ) -> ResultSetModel {
        let schema = Arc::new(Schema::new(fields));
        let b = RecordBatch::try_new(schema.clone(), arrays).unwrap();
        let mut m = ResultSetModel::new(0, 0, schema, 10);
        m.push_batch(b).unwrap();
        m
    }

    #[test]
    fn inspection_offers_the_right_viewers() {
        let json = Field::new("j", DataType::Utf8, true)
            .with_metadata([(ENGINE_TYPE_METADATA_KEY.to_owned(), "jsonb".to_owned())]);
        let m = model(
            vec![
                json,
                Field::new("t", DataType::Utf8, true),
                Field::new("b", DataType::Binary, true),
            ],
            vec![
                Arc::new(StringArray::from(vec![Some(r#"{"k":1}"#), None])),
                Arc::new(StringArray::from(vec![Some("<a><b/></a>"), Some("plain")])),
                Arc::new(BinaryArray::from(vec![
                    Some(png(2, 2).as_slice()),
                    Some(&b"\x00\x01"[..]),
                ])),
            ],
        );
        let j = inspect_cell(&m, 0, 0).unwrap();
        assert_eq!(j.viewers, vec![ViewerKind::Json, ViewerKind::Text]);
        assert_eq!(render(&j, ViewerKind::Json).text, "{\n  \"k\": 1\n}");
        assert!(inspect_cell(&m, 1, 0).unwrap().is_null);
        let x = inspect_cell(&m, 0, 1).unwrap();
        assert_eq!(x.viewers, vec![ViewerKind::Text, ViewerKind::Xml]);
        assert_eq!(
            inspect_cell(&m, 1, 1).unwrap().viewers,
            vec![ViewerKind::Text]
        );
        let img = inspect_cell(&m, 0, 2).unwrap();
        assert_eq!(img.viewers, vec![ViewerKind::Image, ViewerKind::Hex]);
        assert!(image_for_preview(&img).is_ok());
        let bin = inspect_cell(&m, 1, 2).unwrap();
        assert_eq!(bin.viewers, vec![ViewerKind::Hex]);
        assert!(
            render(&bin, ViewerKind::Hex)
                .text
                .starts_with("00000000  00 01")
        );
        assert!(inspect_cell(&m, 5, 0).is_none());
    }

    #[test]
    fn cut_values_are_flagged_and_not_decoded() {
        let field = Field::new("b", DataType::Binary, true)
            .with_metadata([(TRUNCATED_VALUES_METADATA_KEY.to_owned(), "true".to_owned())]);
        let m = model(
            vec![field],
            vec![Arc::new(BinaryArray::from(vec![Some(
                png(2, 2).as_slice(),
            )]))],
        );
        let i = inspect_cell(&m, 0, 0).unwrap();
        assert!(i.cut_by_driver);
        assert_eq!(image_for_preview(&i), Err(ImageRefusal::Cut));
        let out = render(&i, ViewerKind::Hex);
        assert!(
            out.notes[0].contains("cut by the driver"),
            "{:?}",
            out.notes
        );
    }

    #[test]
    fn viewer_text_neutralises_controls() {
        assert_eq!(
            viewer_text("a\tb\r\nc\u{202e}d\u{7}"),
            "a\tb\nc<U+202E>d\u{FFFD}"
        );
    }
}
