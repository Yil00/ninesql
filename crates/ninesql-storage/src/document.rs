//! Versioned documents: every NineSQL file carries a top-level `schema_version` and is migrated
//! forward on load (ADR-0008 §2).
//!
//! Rules:
//! - A file with a **newer** version is refused and left untouched (no silent downgrade).
//! - An **older** file is migrated step by step (`from` → `from + 1`) on a JSON value; the original
//!   bytes are kept as `<file>.bak` and the migrated file is written atomically.
//! - Unknown fields are ignored on read (models must not use `deny_unknown_fields`) and dropped on
//!   the next save. A later NineSQL that adds fields bumps `schema_version`.
//! - Parse errors never echo string values from the file (they could be a misplaced secret).

use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Serialize, Serializer};
use serde_json::Value;

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
/// # Errors
/// I/O, parse, version and migration errors. The file is never modified on error.
pub fn load<D: Document>(path: &Path) -> Result<Option<Loaded<D>>, StorageError> {
    let Some(bytes) = fs::read_private(path, DEFAULT_READ_LIMIT)? else {
        return Ok(None);
    };
    let loaded = decode::<D>(path, &bytes)?;
    if !loaded.secret_like_keys.is_empty() {
        tracing::warn!(
            path = %path.display(),
            keys = ?loaded.secret_like_keys,
            "keys that look like secrets were found and will be dropped on the next save"
        );
    }
    if let Some(from) = loaded.migrated_from {
        let bak = backup_path(path);
        if loaded.secret_like_keys.is_empty() {
            fs::write_atomic(&bak, &bytes)?;
        } else {
            // Never copy a misplaced secret into one more file (I1): the backup keeps everything
            // but the secret-looking keys.
            fs::write_atomic(&bak, &scrubbed_backup::<D>(path, &bytes)?)?;
        }
        save(path, &loaded.document)?;
        tracing::info!(
            path = %path.display(),
            from,
            to = D::SCHEMA_VERSION,
            backup = %bak.display(),
            "migrated workspace file"
        );
    }
    Ok(Some(loaded))
}

/// The original file re-encoded in its own format, without its secret-looking keys.
fn scrubbed_backup<D: Document>(path: &Path, bytes: &[u8]) -> Result<Vec<u8>, StorageError> {
    let err = |detail: String| StorageError::Migration {
        path: path.to_path_buf(),
        from: 0,
        detail: format!("cannot write a backup without its secret-looking keys: {detail}"),
    };
    let mut value: Value = match D::FORMAT {
        Format::Json => serde_json::from_slice(bytes).map_err(|_| err("unreadable".into()))?,
        Format::Toml => std::str::from_utf8(bytes)
            .ok()
            .and_then(|t| toml::from_str(t).ok())
            .ok_or_else(|| err("unreadable".into()))?,
    };
    hygiene::strip_secret_like_keys(&mut value);
    match D::FORMAT {
        Format::Json => {
            let mut out = serde_json::to_vec_pretty(&value).map_err(|e| err(e.to_string()))?;
            out.push(b'\n');
            Ok(out)
        }
        Format::Toml => toml::to_string_pretty(&value)
            .map(String::into_bytes)
            .map_err(|e| err(e.to_string())),
    }
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
/// - backtick-quoted text is replaced by `` `…` `` unless it is a key name (right after
///   `field`) or part of the schema's expectation list (after `expected`), which only hold names
///   from NineSQL's own models.
pub fn redact(message: &str) -> String {
    let mut out = String::with_capacity(message.len());
    let mut chars = message.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        match c {
            '"' => {
                let mut escaped = false;
                for (_, d) in chars.by_ref() {
                    if escaped {
                        escaped = false;
                    } else if d == '\\' {
                        escaped = true;
                    } else if d == '"' {
                        break;
                    }
                }
                out.push_str("\"…\"");
            }
            '`' => {
                let before = &message[..i];
                let keep = before.trim_end().ends_with("field") || before.contains("expected");
                let mut inner = String::new();
                for (_, d) in chars.by_ref() {
                    if d == '`' {
                        break;
                    }
                    inner.push(d);
                }
                out.push('`');
                out.push_str(if keep { &inner } else { "…" });
                out.push('`');
            }
            _ => out.push(c),
        }
    }
    out
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

    #[test]
    fn backup_path_appends_bak() {
        assert_eq!(
            backup_path(Path::new("/w/connections.json")),
            Path::new("/w/connections.json.bak")
        );
    }
}
