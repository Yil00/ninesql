//! Versioned documents: every NineSQL file carries a top-level `schema_version` and is migrated
//! forward on load (ADR-0008 §2).
//!
//! Rules:
//! - A file with a **newer** version is refused and left untouched (no silent downgrade).
//! - An **older** file is migrated step by step (`from` → `from + 1`) on a JSON value; the original
//!   is kept as `<file>.bak` and the migrated file is written atomically. The backup is built from
//!   a **scrubbed** copy of the original: secret-looking keys are stripped and
//!   [`Document::scrub`] removes secret-looking values; when neither changes anything, the
//!   original bytes are kept verbatim.
//! - A file holding secret-looking keys is rewritten **at once** without them (atomic, `0600`),
//!   instead of waiting for the next save (I1).
//! - Unknown fields are ignored on read (models must not use `deny_unknown_fields`) and dropped on
//!   the next save. A later NineSQL that adds fields bumps `schema_version`.
//! - Parse errors never echo string values from the file (they could be a misplaced secret).

use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Serialize, Serializer};
use serde_json::Value;

use zeroize::Zeroizing;

use crate::error::StorageError;
use crate::fs::{self, DEFAULT_READ_LIMIT};
use crate::hygiene;

/// Name of the version key at the top of every file.
pub const SCHEMA_VERSION_KEY: &str = "schema_version";

/// On-disk syntax of a document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// Pretty-printed JSON, for machine-owned files.
    Json,
    /// TOML, for hand-edited settings.
    Toml,
}

impl Format {
    fn name(self) -> &'static str {
        match self {
            Self::Json => "JSON",
            Self::Toml => "TOML",
        }
    }
}

/// One migration step from schema version `from` to `from + 1`.
#[derive(Debug, Clone, Copy)]
pub struct Migration {
    /// Version this step reads.
    pub from: u32,
    /// What the step changes (for logs and reviews).
    pub description: &'static str,
    /// Rewrites the document in place. The `schema_version` key is handled by the caller.
    pub apply: fn(&mut Value) -> Result<(), String>,
}

/// A versioned file model.
pub trait Document: Serialize + DeserializeOwned {
    /// Short name used in errors and logs (`connections`, `settings`…).
    const NAME: &'static str;
    /// On-disk syntax.
    const FORMAT: Format;
    /// Current schema version (≥ 1). Every bump adds a [`Migration`] and a fixture of the
    /// previous version.
    const SCHEMA_VERSION: u32;

    /// Migration steps, one per version from 1 to `SCHEMA_VERSION - 1`, in any order.
    #[must_use]
    fn migrations() -> &'static [Migration] {
        &[]
    }

    /// Removes secret-looking **values** from the raw content of a file at `schema_version`
    /// (which may be older than [`Self::SCHEMA_VERSION`]), in place. Called on the copy written as
    /// the migration backup (`<file>.bak`), after secret-looking keys are stripped, so a misplaced
    /// secret is never copied into one more file (I1).
    ///
    /// The default removes nothing. A model with free-form fields (driver properties, init SQL)
    /// applies the same rules here as on load, and must accept every layout from version 1 on.
    /// Implementations zeroize what they remove ([`hygiene::zeroize_value`]).
    fn scrub(value: &mut Value, schema_version: u32) {
        let _ = (value, schema_version);
    }
}

/// Result of a load.
#[derive(Debug)]
pub struct Loaded<D> {
    /// The document, at the current schema version.
    pub document: D,
    /// Version the file had before migration, if it was migrated.
    pub migrated_from: Option<u32>,
    /// Paths of keys that look like they hold a secret value (see [`crate::hygiene`]).
    pub secret_like_keys: Vec<String>,
}

/// Loads a document, migrating it on disk if it is older. Returns `None` if the file is missing.
///
/// If the file holds secret-looking keys that the model drops (unknown keys), or a JSON object
/// with a duplicate key (whose shadowed value stays in the bytes), it is rewritten at once
/// without them. Secret-looking entries the model keeps (free-form maps) are left to the
/// caller, which sanitizes and saves (see [`Loaded::secret_like_keys`]).
///
/// # Errors
/// I/O, parse, version and migration errors. The file is never modified on error.
pub fn load<D: Document>(path: &Path) -> Result<Option<Loaded<D>>, StorageError> {
    let Some(bytes) = fs::read_private(path, DEFAULT_READ_LIMIT)? else {
        return Ok(None);
    };
    // The raw content may hold a misplaced secret: zeroized when dropped.
    let bytes = Zeroizing::new(bytes);
    let loaded = decode::<D>(path, &bytes)?;
    if let Some(from) = loaded.migrated_from {
        let bak = backup_path(path);
        fs::write_atomic(&bak, &backup_bytes::<D>(path, &bytes, from)?)?;
        save(path, &loaded.document)?;
        tracing::info!(
            file = %fs::log_name(path),
            from,
            to = D::SCHEMA_VERSION,
            backup = %fs::log_name(&bak),
            "migrated workspace file"
        );
    } else {
        let removable = !loaded.secret_like_keys.is_empty() && {
            let remaining = serde_json::to_value(&loaded.document)
                .map(|v| hygiene::find_secret_like_keys(&v).len())
                .unwrap_or(usize::MAX);
            remaining < loaded.secret_like_keys.len()
        };
        // A duplicate JSON key: the parser kept the last value, an earlier one (possibly a
        // secret, `"token": "abc", "token": ""`) is still in the bytes. Re-encoding drops it.
        let duplicate = D::FORMAT == Format::Json && has_duplicate_json_keys(&bytes);
        if removable || duplicate {
            save(path, &loaded.document)?;
            tracing::warn!(
                file = %fs::log_name(path),
                keys = ?loaded.secret_like_keys,
                duplicate_keys = duplicate,
                "rewrote a workspace file without data that may hold a secret"
            );
        }
    }
    if !loaded.secret_like_keys.is_empty() {
        tracing::warn!(
            file = %fs::log_name(path),
            keys = ?loaded.secret_like_keys,
            "keys that look like secrets were found in a workspace file"
        );
    }
    Ok(Some(loaded))
}

/// Content of the migration backup: the original file with secret-looking keys stripped and
/// [`Document::scrub`] applied for `version`, re-encoded in the file's own format.
///
/// JSON is always re-encoded from the scrubbed value: a JSON parser keeps only the last of
/// duplicate keys, so comparing parsed values cannot prove that the original bytes are clean
/// (`"token": "abc", "token": ""`). TOML refuses duplicate keys, so a TOML file whose scrubbed
/// value equals the parsed one is kept verbatim (with its comments and layout).
fn backup_bytes<D: Document>(
    path: &Path,
    bytes: &[u8],
    version: u32,
) -> Result<Zeroizing<Vec<u8>>, StorageError> {
    let err = |detail: String| StorageError::Migration {
        path: path.to_path_buf(),
        from: version,
        detail: format!("cannot write a backup without its secret-looking values: {detail}"),
    };
    let (mut original, mut scrubbed) =
        scrubbed_value::<D>(bytes, Some(version)).ok_or_else(|| err("unreadable".into()))?;
    let out = if D::FORMAT == Format::Toml && scrubbed == original {
        Ok(Zeroizing::new(bytes.to_vec()))
    } else {
        encode_value(D::FORMAT, &scrubbed).map_err(err)
    };
    hygiene::zeroize_value(&mut original);
    hygiene::zeroize_value(&mut scrubbed);
    out
}

/// The parsed file and a copy with secret-looking keys stripped and [`Document::scrub`] applied
/// (for `version`, or else the file's own `schema_version`). `None` if the bytes do not parse,
/// or, when the version is read from the file, if it is missing, invalid or newer than
/// [`Document::SCHEMA_VERSION`]: this build does not know that layout and never rewrites it.
fn scrubbed_value<D: Document>(bytes: &[u8], version: Option<u32>) -> Option<(Value, Value)> {
    let original: Value = match D::FORMAT {
        Format::Json => serde_json::from_slice(bytes).ok()?,
        Format::Toml => toml::from_str(std::str::from_utf8(bytes).ok()?).ok()?,
    };
    let version = match version {
        Some(v) => v,
        None => original
            .get(SCHEMA_VERSION_KEY)
            .and_then(Value::as_u64)
            .and_then(|v| u32::try_from(v).ok())
            .filter(|v| (1..=D::SCHEMA_VERSION).contains(v))?,
    };
    let mut scrubbed = original.clone();
    hygiene::strip_secret_like_keys(&mut scrubbed);
    D::scrub(&mut scrubbed, version);
    Some((original, scrubbed))
}

fn encode_value(format: Format, value: &Value) -> Result<Zeroizing<Vec<u8>>, String> {
    match format {
        Format::Json => serde_json::to_vec_pretty(value)
            .map(|mut out| {
                out.push(b'\n');
                Zeroizing::new(out)
            })
            .map_err(|e| e.to_string()),
        Format::Toml => toml::to_string_pretty(value)
            .map(|t| Zeroizing::new(t.into_bytes()))
            .map_err(|e| e.to_string()),
    }
}

/// Removes secret-looking keys and what [`Document::scrub`] removes from a file **as it is on
/// disk**, without deserializing it into the model, and writes it back atomically (`0600`) if
/// anything was removed. Everything else is kept, including data the model would reject.
///
/// Used when a load fails validation after parsing, so a misplaced secret elsewhere in the file
/// does not stay on disk until the user fixes the invalid data (I1). A JSON file is re-encoded
/// whenever it holds a duplicate key, for the reason given on the migration backup. Returns
/// whether the file was rewritten; a missing or unparsable file, or one without a valid
/// `schema_version` or with a newer one, is left alone (`Ok(false)`).
///
/// # Errors
/// I/O errors.
pub fn scrub_file<D: Document>(path: &Path) -> Result<bool, StorageError> {
    let Some(bytes) = fs::read_private(path, DEFAULT_READ_LIMIT)? else {
        return Ok(false);
    };
    let bytes = Zeroizing::new(bytes);
    let Some((mut original, mut scrubbed)) = scrubbed_value::<D>(&bytes, None) else {
        return Ok(false);
    };
    // Equal values do not prove clean JSON bytes: a duplicate key may hide a secret that the
    // parser dropped in favour of a later value.
    let changed =
        scrubbed != original || (D::FORMAT == Format::Json && has_duplicate_json_keys(&bytes));
    let result = if changed {
        let out = encode_value(D::FORMAT, &scrubbed).map_err(|detail| StorageError::Serialize {
            document: D::NAME,
            detail,
        })?;
        fs::write_atomic(path, &out)?;
        tracing::warn!(file = %fs::log_name(path), "removed secret-looking data from a workspace file that failed to load");
        Ok(true)
    } else {
        Ok(false)
    };
    hygiene::zeroize_value(&mut original);
    hygiene::zeroize_value(&mut scrubbed);
    result
}

/// Whether a JSON document repeats a key inside one object (checked with a streaming visitor).
fn has_duplicate_json_keys(bytes: &[u8]) -> bool {
    use serde::de::{Deserializer, MapAccess, SeqAccess, Visitor};
    use std::collections::HashSet;

    struct Dup;
    impl<'de> Visitor<'de> for Dup {
        type Value = bool;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("any JSON value")
        }
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<bool, A::Error> {
            let mut seen = HashSet::new();
            let mut dup = false;
            while let Some(key) = map.next_key::<String>()? {
                dup |= !seen.insert(key);
                dup |= map.next_value::<DupValue>()?.0;
            }
            Ok(dup)
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<bool, A::Error> {
            let mut dup = false;
            while let Some(v) = seq.next_element::<DupValue>()? {
                dup |= v.0;
            }
            Ok(dup)
        }
        fn visit_bool<E>(self, _: bool) -> Result<bool, E> {
            Ok(false)
        }
        fn visit_i64<E>(self, _: i64) -> Result<bool, E> {
            Ok(false)
        }
        fn visit_u64<E>(self, _: u64) -> Result<bool, E> {
            Ok(false)
        }
        fn visit_f64<E>(self, _: f64) -> Result<bool, E> {
            Ok(false)
        }
        fn visit_str<E>(self, _: &str) -> Result<bool, E> {
            Ok(false)
        }
        fn visit_unit<E>(self) -> Result<bool, E> {
            Ok(false)
        }
        fn visit_string<E>(self, _: String) -> Result<bool, E> {
            Ok(false)
        }
    }
    struct DupValue(bool);
    impl<'de> serde::Deserialize<'de> for DupValue {
        fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            d.deserialize_any(Dup).map(DupValue)
        }
    }
    let mut de = serde_json::Deserializer::from_slice(bytes);
    de.deserialize_any(Dup).unwrap_or(true)
}

/// Saves a document atomically with its `schema_version`. The parent directory must exist.
///
/// # Errors
/// Serialization and I/O errors.
pub fn save<D: Document>(path: &Path, document: &D) -> Result<(), StorageError> {
    let bytes = encode(document)?;
    fs::write_atomic(path, &bytes)
}

/// `<file>.bak`, the copy of a file taken before migrating it.
#[must_use]
pub fn backup_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".bak");
    path.with_file_name(name)
}

/// Serializes a document with `schema_version` as its first key.
///
/// # Errors
/// [`StorageError::Serialize`].
pub fn encode<D: Document>(document: &D) -> Result<Vec<u8>, StorageError> {
    let envelope = Envelope {
        schema_version: D::SCHEMA_VERSION,
        body: document,
    };
    let err = |detail: String| StorageError::Serialize {
        document: D::NAME,
        detail,
    };
    match D::FORMAT {
        Format::Json => {
            let mut bytes = serde_json::to_vec_pretty(&envelope).map_err(|e| err(e.to_string()))?;
            bytes.push(b'\n');
            Ok(bytes)
        }
        Format::Toml => toml::to_string_pretty(&envelope)
            .map(String::into_bytes)
            .map_err(|e| err(e.to_string())),
    }
}

/// Parses and migrates a document held in memory. `source` names it in errors.
///
/// # Errors
/// Parse, version and migration errors.
pub fn decode<D: Document>(source: &Path, bytes: &[u8]) -> Result<Loaded<D>, StorageError> {
    let parse_err = |detail: String| StorageError::Parse {
        path: source.to_path_buf(),
        document: D::NAME,
        detail: redact(&detail),
    };
    let mut value: Value = match D::FORMAT {
        Format::Json => serde_json::from_slice(bytes).map_err(|e| parse_err(e.to_string()))?,
        Format::Toml => {
            let text = std::str::from_utf8(bytes)
                .map_err(|_| parse_err(format!("not valid UTF-8 {}", D::FORMAT.name())))?;
            toml::from_str(text).map_err(|e| parse_err(e.message().to_owned()))?
        }
    };
    let Value::Object(map) = &mut value else {
        return Err(parse_err("the top level must be an object".to_owned()));
    };
    let found = map
        .remove(SCHEMA_VERSION_KEY)
        .and_then(|v| v.as_u64())
        .and_then(|v| u32::try_from(v).ok())
        .filter(|v| *v >= 1)
        .ok_or_else(|| StorageError::MissingSchemaVersion {
            path: source.to_path_buf(),
        })?;
    if found > D::SCHEMA_VERSION {
        return Err(StorageError::NewerVersion {
            path: source.to_path_buf(),
            found,
            supported: D::SCHEMA_VERSION,
        });
    }
    let secret_like_keys = hygiene::find_secret_like_keys(&value);
    for from in found..D::SCHEMA_VERSION {
        let step = D::migrations().iter().find(|m| m.from == from).ok_or(
            StorageError::MissingMigration {
                document: D::NAME,
                from,
            },
        )?;
        tracing::debug!(
            document = D::NAME,
            from,
            step = step.description,
            "migration step"
        );
        (step.apply)(&mut value).map_err(|detail| StorageError::Migration {
            path: source.to_path_buf(),
            from,
            detail,
        })?;
        if !value.is_object() {
            return Err(StorageError::Migration {
                path: source.to_path_buf(),
                from,
                detail: "the step did not produce an object".to_owned(),
            });
        }
    }
    let document = D::deserialize(value).map_err(|e| parse_err(e.to_string()))?;
    Ok(Loaded {
        document,
        migrated_from: (found < D::SCHEMA_VERSION).then_some(found),
        secret_like_keys,
    })
}

/// Serializes `schema_version` first, then the document's own fields.
struct Envelope<'a, D> {
    schema_version: u32,
    body: &'a D,
}

impl<D: Serialize> Serialize for Envelope<'_, D> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::{Error, SerializeMap};
        let Value::Object(body) = serde_json::to_value(self.body).map_err(S::Error::custom)? else {
            return Err(S::Error::custom("a document must serialize to an object"));
        };
        if body.contains_key(SCHEMA_VERSION_KEY) {
            return Err(S::Error::custom(
                "a document must not define `schema_version`",
            ));
        }
        let mut map = serializer.serialize_map(Some(body.len() + 1))?;
        map.serialize_entry(SCHEMA_VERSION_KEY, &self.schema_version)?;
        for (k, v) in &body {
            map.serialize_entry(k, v)?;
        }
        map.end()
    }
}

/// Redacts quoted text in a parser message so a misplaced secret in the file never reaches an
/// error message or a log (I1):
/// - double-quoted literals (`invalid type: string "…"`) are always replaced by `"…"`;
/// - backtick-quoted text is replaced by `` `…` `` unless it is
///   - an identifier-like (`[A-Za-z0-9_-]+`) key name that is the first quoted text of the
///     message, right after exactly `unknown field `, `missing field ` or `duplicate field `, or
///   - an item of the schema's expectation list: the contiguous list that starts right after
///     `expected` or `expected one of`, continues only through `, `, ` or ` and `, or `
///     separators, and **runs to the end of the message** (optionally followed by serde_json's
///     ` at line N column M`). Every item must be identifier-like, otherwise the whole list is
///     redacted.
///
/// The end-of-message rule matters because the rejected value comes first in serde messages
/// (`unknown variant `…`, expected …`): a value that embeds backticks and "expected" can forge a
/// list, but that forged list is always followed by serde's own text, never by the end.
pub fn redact(message: &str) -> String {
    let segments = segments(message);
    let mut keep = vec![false; segments.len()];
    let prefix_text = |i: usize| -> &str {
        match i.checked_sub(1).map(|j| &segments[j]) {
            Some(Segment::Text(t)) => t.trim_end(),
            _ => "",
        }
    };
    let mut i = 0;
    while i < segments.len() {
        let Segment::Tick { inner, closed } = &segments[i] else {
            i += 1;
            continue;
        };
        let before = prefix_text(i);
        if before.ends_with("expected") || before.ends_with("expected one of") {
            // Collect the list: ticks separated by separator-only text.
            let mut items = vec![i];
            let mut j = i + 1;
            while j + 1 < segments.len()
                && matches!(&segments[j], Segment::Text(t) if matches!(*t, ", " | " or " | ", or "))
                && matches!(segments[j + 1], Segment::Tick { .. })
            {
                items.push(j + 1);
                j += 2;
            }
            let last = items[items.len() - 1];
            let at_end = match segments.get(last + 1) {
                None => true,
                Some(Segment::Text(t)) => last + 2 == segments.len() && is_position_suffix(t),
                Some(_) => false,
            };
            let all_ok = items.iter().all(|&k| {
                matches!(&segments[k], Segment::Tick { inner, closed: true } if is_identifier(inner))
            });
            if at_end && all_ok {
                for k in &items {
                    keep[*k] = true;
                }
            }
            i = last + 1;
            continue;
        }
        // A key name only in serde's own `unknown / missing / duplicate field `x`` opening: the
        // first quoted segment of the message, right after exactly that text. Anywhere else the
        // word "field" may come from a rejected value.
        let field_opening = i == 1
            && matches!(
                &segments[0],
                Segment::Text(t) if matches!(*t, "unknown field " | "missing field " | "duplicate field ")
            );
        if field_opening && *closed && is_identifier(inner) {
            keep[i] = true;
        }
        i += 1;
    }
    let mut out = String::with_capacity(message.len());
    for (segment, keep) in segments.iter().zip(keep) {
        match segment {
            Segment::Text(t) => out.push_str(t),
            Segment::Quoted => out.push_str("\"…\""),
            Segment::Tick { inner, .. } => {
                out.push('`');
                out.push_str(if keep { inner } else { "…" });
                out.push('`');
            }
        }
    }
    out
}

/// A piece of a parser message.
enum Segment<'a> {
    /// Text outside quotes.
    Text(&'a str),
    /// A double-quoted literal (always redacted).
    Quoted,
    /// Backtick-quoted text; `closed` is false when the message ends inside it.
    Tick { inner: &'a str, closed: bool },
}

fn segments(message: &str) -> Vec<Segment<'_>> {
    let mut out = Vec::new();
    let mut chars = message.char_indices();
    let mut text_start = 0;
    while let Some((i, c)) = chars.next() {
        match c {
            '"' => {
                if text_start < i {
                    out.push(Segment::Text(&message[text_start..i]));
                }
                let mut escaped = false;
                let mut end = message.len();
                for (j, d) in chars.by_ref() {
                    if escaped {
                        escaped = false;
                    } else if d == '\\' {
                        escaped = true;
                    } else if d == '"' {
                        end = j + 1;
                        break;
                    }
                }
                out.push(Segment::Quoted);
                text_start = end;
            }
            '`' => {
                if text_start < i {
                    out.push(Segment::Text(&message[text_start..i]));
                }
                let mut end = None;
                for (j, d) in chars.by_ref() {
                    if d == '`' {
                        end = Some(j);
                        break;
                    }
                }
                match end {
                    Some(j) => {
                        out.push(Segment::Tick {
                            inner: &message[i + 1..j],
                            closed: true,
                        });
                        text_start = j + 1;
                    }
                    None => {
                        out.push(Segment::Tick {
                            inner: &message[i + 1..],
                            closed: false,
                        });
                        text_start = message.len();
                    }
                }
            }
            _ => {}
        }
    }
    if text_start < message.len() {
        out.push(Segment::Text(&message[text_start..]));
    }
    out
}

/// `[A-Za-z0-9_-]+`: the shape of every name in NineSQL's models.
fn is_identifier(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// ` at line N column M`, as appended by serde_json.
fn is_position_suffix(t: &str) -> bool {
    let Some(rest) = t.strip_prefix(" at line ") else {
        return false;
    };
    let Some((line, column)) = rest.split_once(" column ") else {
        return false;
    };
    [line, column]
        .iter()
        .all(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).
mod tests {
    use super::*;

    #[test]
    fn redacts_quoted_values() {
        assert_eq!(
            redact(r#"invalid type: string "hunter2", expected u16"#),
            r#"invalid type: string "…", expected u16"#
        );
        assert_eq!(redact(r#"a "b\"c" d"#), r#"a "…" d"#);
        assert_eq!(redact(r#"open "abc"#), r#"open "…""#);
        assert_eq!(redact("unknown field `x`"), "unknown field `x`");
        assert_eq!(
            redact("unknown variant `Hunter2-Secret`, expected one of `tcp`, `file`"),
            "unknown variant `…`, expected one of `tcp`, `file`"
        );
        assert_eq!(
            redact("invalid engine identifier `Hunter2-Secret`"),
            "invalid engine identifier `…`"
        );
        assert_eq!(
            redact("missing field `connection_type`"),
            "missing field `connection_type`"
        );
        assert_eq!(redact("open `abc"), "open `…`");
    }

    /// N1: only the contiguous list right after `expected` / `expected one of` is kept.
    #[test]
    fn keeps_only_the_expectation_list() {
        assert_eq!(
            redact("unknown variant `x`, expected `tcp` or `file`"),
            "unknown variant `…`, expected `tcp` or `file`"
        );
        assert_eq!(
            redact("expected one of `a`, `b`, or `c`"),
            "expected one of `a`, `b`, or `c`"
        );
        assert_eq!(
            redact("unknown field `x`, expected one of `a`, `b`"),
            "unknown field `x`, expected one of `a`, `b`"
        );
        // Backtick text after the list, or later in a message that mentions "expected", is
        // redacted.
        assert_eq!(
            redact("expected `a` at line 1 near `Hunter2-Secret`"),
            "expected `…` at line 1 near `…`"
        );
        assert_eq!(
            redact("expected a string, found `Hunter2-Secret`"),
            "expected a string, found `…`"
        );
        assert_eq!(
            redact("invalid value `Hunter2-Secret` where `a` was expected, near `s3cret`"),
            "invalid value `…` where `…` was expected, near `…`"
        );
        assert_eq!(
            redact("expected `a`, \"x\" `Hunter2`"),
            "expected `…`, \"…\" `…`"
        );
        // serde_json's position suffix may follow the list.
        assert_eq!(
            redact("unknown variant `x`, expected `tcp` or `file` at line 3 column 17"),
            "unknown variant `…`, expected `tcp` or `file` at line 3 column 17"
        );
    }

    /// Follow-up review N-1: "field" inside a rejected value does not make the next quoted text a
    /// key name.
    #[test]
    fn forged_field_names_are_redacted() {
        // The enum value is: x` field `Hunter2
        assert_eq!(
            redact("unknown variant `x` field `Hunter2`, expected one of `tcp`, `file`"),
            "unknown variant `…` field `…`, expected one of `tcp`, `file`"
        );
        // The key is: x` missing field `Hunter2
        assert_eq!(
            redact(
                "unknown field `x` missing field `Hunter2`, expected `a` or `b` at line 1 column 2"
            ),
            "unknown field `x` missing field `…`, expected `a` or `b` at line 1 column 2"
        );
        assert_eq!(
            redact("invalid type: field `Hunter2`"),
            "invalid type: field `…`"
        );
        for opening in ["unknown", "missing", "duplicate"] {
            assert_eq!(
                redact(&format!("{opening} field `connection_type`")),
                format!("{opening} field `connection_type`")
            );
        }
    }

    /// Security review L1: a value embedding backticks and "expected" cannot forge a kept list.
    #[test]
    fn forged_expectation_lists_are_redacted() {
        // The enum value is: x`, expected `Hunter2
        assert_eq!(
            redact("unknown variant `x`, expected `Hunter2`, expected one of `tcp`, `file`"),
            "unknown variant `…`, expected `…`, expected one of `tcp`, `file`"
        );
        assert_eq!(
            redact("unknown field `x`, expected `Hunter2`, expected one of `a`, `b`"),
            "unknown field `x`, expected `…`, expected one of `a`, `b`"
        );
        // A forged list before serde's "there are no …" tail.
        assert_eq!(
            redact("unknown variant `x`, expected `Hunter2`, there are no variants"),
            "unknown variant `…`, expected `…`, there are no variants"
        );
        // Trying to glue the forged item onto the real list.
        assert_eq!(
            redact("unknown variant `x`, expected `Hunter2`, ``, expected one of `tcp`"),
            "unknown variant `…`, expected `…`, `…`, expected one of `tcp`"
        );
        // Odd backticks shift the quoting: the real list then fails the identifier check.
        assert_eq!(
            redact("unknown variant ```, expected one of `tcp`, `file`"),
            "unknown variant `…``…`tcp`…`file`…`"
        );
        // Non-identifier items or field names are redacted.
        assert_eq!(
            redact("expected one of `a`, `Hunter 2!`"),
            "expected one of `…`, `…`"
        );
        assert_eq!(
            redact("unknown field `p@ss word`, expected `a`"),
            "unknown field `…`, expected `a`"
        );
    }

    #[test]
    fn backup_path_appends_bak() {
        assert_eq!(
            backup_path(Path::new("/w/connections.json")),
            Path::new("/w/connections.json.bak")
        );
    }
}
