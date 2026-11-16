//! SQL snippets (F-SQL-10): named templates with placeholders, per project, stored in
//! `snippets.json` (ADR-0008: versioned JSON, written atomically, `0600`). No UI yet: the
//! editor lists them with [`SnippetStore::list`] and inserts [`expand`]'s text.
//!
//! # Placeholders
//! `${name}` is replaced by the value given for `name`; `${name:default}` falls back to
//! `default`; `${cursor}` marks where the caret goes after insertion (it expands to nothing);
//! `$$` is a literal `$`. Any other `$` is copied. Values are inserted **as typed**: an expanded
//! snippet is editor text the user reads, edits and runs like any other statement (with its
//! classification and confirmations), never executed by the snippet code.
//!
//! # Secrets (I1)
//! A snippet is a workspace file, so it must never hold a password: a body holding a URI with
//! credentials (`postgresql://u:pw@h`) is refused, and so is a body that holds a credential word
//! (`PASSWORD`, `IDENTIFIED`, `SECRET`, `TOKEN`… the history rule,
//! [`ninesql_driver_api::text_may_hold_credentials`]) together with a string literal. Use a
//! placeholder for the credential instead: `CREATE USER ${user} IDENTIFIED BY ${password}`.

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;

use ninesql_storage::document::{self, Document, Format};
use ninesql_storage::{ProjectPaths, StorageError};
use serde::{Deserialize, Serialize};

/// `schema_version` of `snippets.json`.
pub const SNIPPETS_SCHEMA_VERSION: u32 = 1;
/// Longest snippet name, in bytes.
pub const MAX_SNIPPET_NAME: usize = 128;
/// Longest snippet body, in bytes.
pub const MAX_SNIPPET_BODY: usize = 64 * 1024;
/// Most snippets per project.
pub const MAX_SNIPPETS: usize = 1000;

/// One snippet. `Debug` shows the name and the body length only.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snippet {
    /// Unique name (case-insensitive) within the project; also what the user types to find it.
    pub name: String,
    /// Body with placeholders.
    pub body: String,
    /// Engine the snippet is for (`postgresql`, `mysql`…), or none for every engine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine: Option<String>,
    /// Short description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl fmt::Debug for Snippet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Snippet")
            .field("name", &self.name)
            .field("body_len", &self.body.len())
            .field("engine", &self.engine)
            .finish_non_exhaustive()
    }
}

impl Snippet {
    /// A snippet for every engine.
    #[must_use]
    pub fn new(name: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            body: body.into(),
            engine: None,
            description: None,
        }
    }

    /// Restricts it to one engine.
    #[must_use]
    pub fn for_engine(mut self, engine: impl Into<String>) -> Self {
        self.engine = Some(engine.into());
        self
    }

    /// The placeholders of the body, in order of first appearance.
    #[must_use]
    pub fn placeholders(&self) -> Vec<Placeholder> {
        let mut out: Vec<Placeholder> = Vec::new();
        for piece in parse(&self.body) {
            if let Piece::Placeholder { name, default } = piece
                && name != "cursor"
                && !out.iter().any(|p| p.name == name)
            {
                out.push(Placeholder {
                    name: name.to_owned(),
                    default: default.map(str::to_owned),
                });
            }
        }
        out
    }
}

/// A placeholder of a snippet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placeholder {
    /// Name.
    pub name: String,
    /// Default value, if any.
    pub default: Option<String>,
}

/// An invalid snippet or a storage error.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SnippetError {
    /// The name is empty, too long or holds a control character.
    #[error("invalid snippet name")]
    InvalidName,
    /// The body is empty or too long.
    #[error("the snippet body is empty or longer than 64 KiB")]
    InvalidBody,
    /// The body looks like it holds a credential (I1).
    #[error(
        "the snippet seems to hold a password or another credential: use a placeholder such as ${{password}} instead"
    )]
    HoldsCredential,
    /// Too many snippets.
    #[error("a project holds at most 1000 snippets")]
    TooMany,
    /// Reading or writing `snippets.json` failed.
    #[error(transparent)]
    Storage(#[from] StorageError),
}

/// On-disk form of `snippets.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SnippetsFile {
    /// The snippets.
    #[serde(default)]
    pub snippets: Vec<Snippet>,
}

impl Document for SnippetsFile {
    const NAME: &'static str = "snippets";
    const FORMAT: Format = Format::Json;
    const SCHEMA_VERSION: u32 = SNIPPETS_SCHEMA_VERSION;
}

fn validate(s: &Snippet) -> Result<(), SnippetError> {
    let name = s.name.trim();
    if name.is_empty() || s.name.len() > MAX_SNIPPET_NAME || s.name.chars().any(char::is_control) {
        return Err(SnippetError::InvalidName);
    }
    if s.body.trim().is_empty() || s.body.len() > MAX_SNIPPET_BODY {
        return Err(SnippetError::InvalidBody);
    }
    let literal = s.body.contains('\'') || s.body.contains('"');
    if ninesql_driver_api::hygiene::has_uri_credentials(&s.body)
        || (literal && ninesql_driver_api::text_may_hold_credentials(&s.body))
    {
        return Err(SnippetError::HoldsCredential);
    }
    Ok(())
}

/// The snippets of one project.
#[derive(Debug, Clone, Default)]
pub struct SnippetStore {
    path: Option<PathBuf>,
    snippets: Vec<Snippet>,
}

impl SnippetStore {
    /// A store kept in memory only (tests, no workspace).
    #[must_use]
    pub fn in_memory() -> Self {
        Self::default()
    }

    /// Loads the project's `snippets.json` (empty when missing). Snippets that fail validation
    /// (a hand-edited file) are dropped and returned by name.
    ///
    /// # Errors
    /// Storage errors (unreadable file, newer schema).
    pub fn load(project: &ProjectPaths) -> Result<(Self, Vec<String>), SnippetError> {
        let path = project.snippets_file();
        let file = document::load::<SnippetsFile>(&path)?
            .map(|l| l.document)
            .unwrap_or_default();
        let mut store = Self {
            path: Some(path),
            snippets: Vec::new(),
        };
        let mut dropped = Vec::new();
        for s in file.snippets {
            if validate(&s).is_ok()
                && store.get(&s.name).is_none()
                && store.snippets.len() < MAX_SNIPPETS
            {
                store.snippets.push(s);
            } else {
                dropped.push(s.name.chars().take(MAX_SNIPPET_NAME).collect());
            }
        }
        Ok((store, dropped))
    }

    /// Every snippet usable with `engine` (its own and the engine-less ones), by name; all of
    /// them when `engine` is `None`.
    #[must_use]
    pub fn list(&self, engine: Option<&str>) -> Vec<&Snippet> {
        let mut out: Vec<&Snippet> = self
            .snippets
            .iter()
            .filter(|s| match (engine, s.engine.as_deref()) {
                (None, _) | (_, None) => true,
                (Some(e), Some(se)) => e.eq_ignore_ascii_case(se),
            })
            .collect();
        out.sort_by_key(|s| s.name.to_lowercase());
        out
    }

    /// The snippet named `name` (case-insensitive).
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Snippet> {
        self.snippets
            .iter()
            .find(|s| s.name.eq_ignore_ascii_case(name.trim()))
    }

    /// Adds or replaces (by name) a snippet. Call [`Self::save`] to write the file.
    ///
    /// # Errors
    /// [`SnippetError`] validation errors.
    pub fn upsert(&mut self, snippet: Snippet) -> Result<(), SnippetError> {
        validate(&snippet)?;
        if let Some(slot) = self
            .snippets
            .iter_mut()
            .find(|s| s.name.eq_ignore_ascii_case(snippet.name.trim()))
        {
            *slot = snippet;
            return Ok(());
        }
        if self.snippets.len() >= MAX_SNIPPETS {
            return Err(SnippetError::TooMany);
        }
        self.snippets.push(snippet);
        Ok(())
    }

    /// Removes a snippet; `true` when it existed.
    pub fn remove(&mut self, name: &str) -> bool {
        let before = self.snippets.len();
        self.snippets
            .retain(|s| !s.name.eq_ignore_ascii_case(name.trim()));
        before != self.snippets.len()
    }

    /// Writes `snippets.json` atomically (nothing for an in-memory store).
    ///
    /// # Errors
    /// Storage errors.
    pub fn save(&self) -> Result<(), SnippetError> {
        if let Some(path) = &self.path {
            if let Some(dir) = path.parent() {
                ninesql_storage::fs::ensure_private_dir(dir)?;
            }
            document::save(
                path,
                &SnippetsFile {
                    snippets: self.snippets.clone(),
                },
            )?;
        }
        Ok(())
    }
}

enum Piece<'a> {
    Text(&'a str),
    Dollar,
    Placeholder {
        name: &'a str,
        default: Option<&'a str>,
    },
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn parse(body: &str) -> Vec<Piece<'_>> {
    let mut out = Vec::new();
    let mut rest = body;
    while let Some(pos) = rest.find('$') {
        if pos > 0 {
            out.push(Piece::Text(&rest[..pos]));
        }
        let after = &rest[pos + 1..];
        if let Some(tail) = after.strip_prefix('$') {
            out.push(Piece::Dollar);
            rest = tail;
            continue;
        }
        if let Some(inner) = after.strip_prefix('{')
            && let Some(close) = inner.find('}')
        {
            let spec = &inner[..close];
            let (name, default) = match spec.split_once(':') {
                Some((n, d)) => (n, Some(d)),
                None => (spec, None),
            };
            if valid_name(name) {
                out.push(Piece::Placeholder { name, default });
                rest = &inner[close + 1..];
                continue;
            }
        }
        out.push(Piece::Text("$"));
        rest = after;
    }
    if !rest.is_empty() {
        out.push(Piece::Text(rest));
    }
    out
}

/// An expanded snippet.
#[derive(Clone, PartialEq, Eq)]
pub struct Expansion {
    /// The text to insert.
    pub text: String,
    /// Byte offset of `${cursor}` in `text`, if the body has one.
    pub cursor: Option<usize>,
    /// Placeholders with no value and no default (left empty).
    pub missing: Vec<String>,
}

impl fmt::Debug for Expansion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Expansion")
            .field("text_len", &self.text.len())
            .field("cursor", &self.cursor)
            .field("missing", &self.missing)
            .finish()
    }
}

/// Expands `snippet` with `values` (see the [module documentation](self)).
#[must_use]
pub fn expand(snippet: &Snippet, values: &HashMap<String, String>) -> Expansion {
    let mut text = String::with_capacity(snippet.body.len());
    let mut cursor = None;
    let mut missing: Vec<String> = Vec::new();
    for piece in parse(&snippet.body) {
        match piece {
            Piece::Text(t) => text.push_str(t),
            Piece::Dollar => text.push('$'),
            Piece::Placeholder { name: "cursor", .. } => {
                cursor.get_or_insert(text.len());
            }
            Piece::Placeholder { name, default } => match (values.get(name), default) {
                (Some(v), _) => text.push_str(v),
                (None, Some(d)) => text.push_str(d),
                (None, None) => {
                    if !missing.iter().any(|m| m == name) {
                        missing.push(name.to_owned());
                    }
                }
            },
        }
    }
    Expansion {
        text,
        cursor,
        missing,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_placeholders() {
        let s = Snippet::new(
            "sel",
            "SELECT ${cols:*} FROM ${table} WHERE price > $$5 AND x = $1 ${cursor}-- ${table}",
        );
        assert_eq!(
            s.placeholders(),
            vec![
                Placeholder {
                    name: "cols".into(),
                    default: Some("*".into())
                },
                Placeholder {
                    name: "table".into(),
                    default: None
                },
            ]
        );
        let mut v = HashMap::new();
        v.insert("table".to_owned(), "orders".to_owned());
        let e = expand(&s, &v);
        assert_eq!(
            e.text,
            "SELECT * FROM orders WHERE price > $5 AND x = $1 -- orders"
        );
        assert_eq!(
            e.cursor,
            Some("SELECT * FROM orders WHERE price > $5 AND x = $1 ".len())
        );
        assert!(e.missing.is_empty());
        let e = expand(&s, &HashMap::new());
        assert_eq!(e.missing, vec!["table"]);
        // Dollar quotes and unknown `${` stay as typed.
        let s = Snippet::new("fn", "$body$ ${not valid} ${");
        assert_eq!(expand(&s, &HashMap::new()).text, "$body$ ${not valid} ${");
    }

    #[test]
    fn saved_and_loaded_from_the_project() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = ninesql_storage::Paths::new(&ninesql_storage::XdgDirs {
            config_home: tmp.path().join("c"),
            data_home: tmp.path().join("d"),
            state_home: tmp.path().join("s"),
        });
        let project = paths.project(&ninesql_storage::ProjectName::default_project());
        let (mut store, dropped) = SnippetStore::load(&project).unwrap();
        assert!(dropped.is_empty() && store.list(None).is_empty());
        store
            .upsert(Snippet::new("top", "SELECT * FROM ${table} LIMIT ${n:10}").for_engine("mysql"))
            .unwrap();
        store.save().unwrap();
        let text = std::fs::read_to_string(project.snippets_file()).unwrap();
        assert!(text.contains("\"schema_version\": 1"));
        let (store, dropped) = SnippetStore::load(&project).unwrap();
        assert!(dropped.is_empty());
        assert_eq!(store.get("TOP").unwrap().engine.as_deref(), Some("mysql"));
    }

    #[test]
    fn validation_refuses_credentials() {
        let mut store = SnippetStore::in_memory();
        assert!(matches!(
            store.upsert(Snippet::new("u", "CREATE USER x IDENTIFIED BY 'hunter2'")),
            Err(SnippetError::HoldsCredential)
        ));
        assert!(matches!(
            store.upsert(Snippet::new("l", "SELECT dblink('postgresql://u:pw@h/db')")),
            Err(SnippetError::HoldsCredential)
        ));
        store
            .upsert(Snippet::new(
                "u",
                "CREATE USER ${user} IDENTIFIED BY ${password}",
            ))
            .unwrap();
        assert!(matches!(
            store.upsert(Snippet::new(" ", "x")),
            Err(SnippetError::InvalidName)
        ));
        assert!(matches!(
            store.upsert(Snippet::new("e", " ")),
            Err(SnippetError::InvalidBody)
        ));
        store
            .upsert(Snippet::new("pg", "SELECT 1").for_engine("postgresql"))
            .unwrap();
        store.upsert(Snippet::new("U", "SELECT 2")).unwrap();
        assert_eq!(store.list(Some("mysql")).len(), 1);
        assert_eq!(store.list(Some("PostgreSQL")).len(), 2);
        assert_eq!(store.get("u").unwrap().body, "SELECT 2");
        assert!(store.remove("PG"));
        assert_eq!(store.list(None).len(), 1);
    }
}
