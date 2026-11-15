//! Script files linked to a connection (F-SQL-11) and editor session restore (F-WS-06): models
//! and their JSON form. Reading and writing the files is the storage layer's job (ADR-0008:
//! `scripts/` in the project, the window layout in `$XDG_STATE_HOME/ninesql`); the editor never
//! does file I/O on the UI thread (I6).
//!
//! The session file holds **references only** (script paths, connection ids, cursor
//! positions), never editor text: an unsaved buffer could hold a password typed in a statement,
//! and I1 forbids writing it anywhere the user did not choose. Unsaved untitled tabs are not
//! restored (an ADR candidate if autosave of scratch scripts is wanted, as DBeaver does).

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use ninesql_core::ConnectionId;
use serde::{Deserialize, Serialize};

/// Errors of the script and session models.
#[derive(Debug, thiserror::Error)]
pub enum ScriptError {
    /// A script path is absolute, leaves the scripts folder or is not a `.sql` file.
    #[error(
        "invalid script path {0:?}: it must be a relative `.sql` path inside the scripts folder"
    )]
    InvalidPath(PathBuf),
    /// The file was written by a newer NineSQL.
    #[error("{what} schema {found} is newer than this NineSQL supports ({supported})")]
    NewerSchema {
        /// Which file.
        what: &'static str,
        /// Found schema.
        found: u32,
        /// Supported schema.
        supported: u32,
    },
    /// Not valid JSON for the model.
    #[error("{what} is not valid: {source}")]
    Json {
        /// Which file.
        what: &'static str,
        /// Cause.
        #[source]
        source: serde_json::Error,
    },
}

/// A script path relative to the project's `scripts/` folder, validated: relative, no `..`,
/// no root or prefix, ends in `.sql`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "PathBuf", into = "PathBuf")]
pub struct ScriptPath(PathBuf);

impl ScriptPath {
    /// Validates `path`.
    ///
    /// # Errors
    /// [`ScriptError::InvalidPath`].
    pub fn new(path: impl Into<PathBuf>) -> Result<Self, ScriptError> {
        let path: PathBuf = path.into();
        let normal = path.components().all(|c| matches!(c, Component::Normal(_)));
        let sql = path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("sql"));
        if path.as_os_str().is_empty() || !normal || !sql {
            return Err(ScriptError::InvalidPath(path));
        }
        Ok(Self(path))
    }

    /// The relative path.
    #[must_use]
    pub fn as_path(&self) -> &Path {
        &self.0
    }

    /// The file name, for tab titles.
    #[must_use]
    pub fn file_name(&self) -> String {
        self.0
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

impl TryFrom<PathBuf> for ScriptPath {
    type Error = ScriptError;
    fn try_from(path: PathBuf) -> Result<Self, Self::Error> {
        Self::new(path)
    }
}

impl From<ScriptPath> for PathBuf {
    fn from(p: ScriptPath) -> Self {
        p.0
    }
}

/// Current schema of [`ScriptLinks`].
pub const SCRIPT_LINKS_SCHEMA: u32 = 1;

/// Which connection each script of a project runs on (`scripts.json` next to `scripts/`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptLinks {
    schema: u32,
    links: BTreeMap<ScriptPath, ConnectionId>,
}

impl ScriptLinks {
    /// No links.
    #[must_use]
    pub fn new() -> Self {
        Self {
            schema: SCRIPT_LINKS_SCHEMA,
            links: BTreeMap::new(),
        }
    }

    /// The connection of `script`.
    #[must_use]
    pub fn connection_for(&self, script: &ScriptPath) -> Option<ConnectionId> {
        self.links.get(script).copied()
    }

    /// Links `script` to `connection` (`None` unlinks).
    pub fn set(&mut self, script: ScriptPath, connection: Option<ConnectionId>) {
        match connection {
            Some(c) => {
                self.links.insert(script, c);
            }
            None => {
                self.links.remove(&script);
            }
        }
    }

    /// A script was renamed or moved.
    pub fn rename(&mut self, from: &ScriptPath, to: ScriptPath) {
        if let Some(c) = self.links.remove(from) {
            self.links.insert(to, c);
        }
    }

    /// A connection was deleted: its scripts are unlinked. Returns the scripts unlinked.
    pub fn forget_connection(&mut self, connection: ConnectionId) -> Vec<ScriptPath> {
        let gone: Vec<ScriptPath> = self
            .links
            .iter()
            .filter(|(_, c)| **c == connection)
            .map(|(p, _)| p.clone())
            .collect();
        for p in &gone {
            self.links.remove(p);
        }
        gone
    }

    /// Every link.
    pub fn iter(&self) -> impl Iterator<Item = (&ScriptPath, &ConnectionId)> {
        self.links.iter()
    }

    /// JSON form.
    ///
    /// # Errors
    /// Serialization failure (not expected for this model).
    pub fn to_json(&self) -> Result<String, ScriptError> {
        serde_json::to_string_pretty(self).map_err(|source| ScriptError::Json {
            what: "scripts.json",
            source,
        })
    }

    /// Parses the JSON form; refuses a newer schema rather than dropping what it cannot read.
    ///
    /// # Errors
    /// [`ScriptError::NewerSchema`], [`ScriptError::Json`] (including invalid script paths).
    pub fn from_json(json: &str) -> Result<Self, ScriptError> {
        let links: Self = serde_json::from_str(json).map_err(|source| ScriptError::Json {
            what: "scripts.json",
            source,
        })?;
        if links.schema > SCRIPT_LINKS_SCHEMA {
            return Err(ScriptError::NewerSchema {
                what: "scripts.json",
                found: links.schema,
                supported: SCRIPT_LINKS_SCHEMA,
            });
        }
        Ok(links)
    }
}

/// Current schema of [`EditorSession`].
pub const SESSION_SCHEMA: u32 = 1;

/// One restorable editor tab: a saved script, its connection and the cursor. No text.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionTab {
    /// The script shown, relative to the project's `scripts/`.
    pub script: ScriptPath,
    /// The connection the tab was using (may differ from the script's link).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection: Option<ConnectionId>,
    /// 0-based cursor line.
    #[serde(default)]
    pub cursor_line: usize,
    /// 0-based cursor column, in `char`s.
    #[serde(default)]
    pub cursor_column: usize,
}

/// The editor tabs of one window, to reopen at the next start.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditorSession {
    schema: u32,
    /// Tabs in order.
    pub tabs: Vec<SessionTab>,
    /// Index of the active tab.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<usize>,
}

impl Default for EditorSession {
    fn default() -> Self {
        Self {
            schema: SESSION_SCHEMA,
            tabs: Vec::new(),
            active: None,
        }
    }
}

impl EditorSession {
    /// JSON form.
    ///
    /// # Errors
    /// Serialization failure (not expected for this model).
    pub fn to_json(&self) -> Result<String, ScriptError> {
        serde_json::to_string_pretty(self).map_err(|source| ScriptError::Json {
            what: "editor session",
            source,
        })
    }

    /// Parses the JSON form. A newer schema is refused; an active index out of range is
    /// dropped.
    ///
    /// # Errors
    /// [`ScriptError::NewerSchema`], [`ScriptError::Json`].
    pub fn from_json(json: &str) -> Result<Self, ScriptError> {
        let mut session: Self = serde_json::from_str(json).map_err(|source| ScriptError::Json {
            what: "editor session",
            source,
        })?;
        if session.schema > SESSION_SCHEMA {
            return Err(ScriptError::NewerSchema {
                what: "editor session",
                found: session.schema,
                supported: SESSION_SCHEMA,
            });
        }
        if session.active.is_some_and(|a| a >= session.tabs.len()) {
            session.active = None;
        }
        Ok(session)
    }

    /// Tabs whose script still exists (`exists` asks the storage layer), with their original
    /// active tab kept when it survives.
    #[must_use]
    pub fn retain_existing(mut self, exists: impl Fn(&ScriptPath) -> bool) -> Self {
        let active_script = self
            .active
            .and_then(|a| self.tabs.get(a))
            .map(|t| t.script.clone());
        self.tabs.retain(|t| exists(&t.script));
        self.active = active_script.and_then(|s| self.tabs.iter().position(|t| t.script == s));
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_paths_stay_inside_the_folder() {
        assert!(ScriptPath::new("reports/monthly.sql").is_ok());
        assert!(ScriptPath::new("Q.SQL").is_ok());
        for bad in [
            "/etc/passwd.sql",
            "../x.sql",
            "a/../../x.sql",
            "x.txt",
            "",
            "./x.sql",
        ] {
            assert!(ScriptPath::new(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn links_round_trip_and_follow_renames() {
        let c = ConnectionId::new_random();
        let mut links = ScriptLinks::new();
        let a = ScriptPath::new("a.sql").ok();
        let b = ScriptPath::new("dir/b.sql").ok();
        let (Some(a), Some(b)) = (a, b) else {
            panic!("paths")
        };
        links.set(a.clone(), Some(c));
        links.rename(&a, b.clone());
        assert_eq!(links.connection_for(&a), None);
        assert_eq!(links.connection_for(&b), Some(c));
        let json = links.to_json().unwrap_or_default();
        let back = ScriptLinks::from_json(&json);
        assert_eq!(back.ok(), Some(links.clone()));
        assert_eq!(links.forget_connection(c), vec![b]);
    }

    #[test]
    fn refuses_newer_schema_and_bad_paths() {
        let newer = r#"{"schema": 2, "links": {}}"#;
        assert!(matches!(
            ScriptLinks::from_json(newer),
            Err(ScriptError::NewerSchema { .. })
        ));
        let escaping =
            r#"{"schema": 1, "links": {"../x.sql": "6a2f41a3-c54c-4b62-9b7a-6d8e0a6f1f0e"}}"#;
        assert!(matches!(
            ScriptLinks::from_json(escaping),
            Err(ScriptError::Json { .. })
        ));
        let newer = r#"{"schema": 9, "tabs": []}"#;
        assert!(matches!(
            EditorSession::from_json(newer),
            Err(ScriptError::NewerSchema { .. })
        ));
    }

    #[test]
    fn session_holds_references_only() {
        let tab = |p: &str| SessionTab {
            script: ScriptPath::new(p).unwrap_or_else(|_| panic!("path")),
            connection: Some(ConnectionId::new_random()),
            cursor_line: 3,
            cursor_column: 7,
        };
        let session = EditorSession {
            tabs: vec![tab("a.sql"), tab("b.sql"), tab("c.sql")],
            active: Some(1),
            ..EditorSession::default()
        };
        let json = session.to_json().unwrap_or_default();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap_or_default();
        let mut keys: Vec<&String> = value["tabs"][0]
            .as_object()
            .map(|o| o.keys().collect())
            .unwrap_or_default();
        keys.sort();
        assert_eq!(
            keys,
            ["connection", "cursor_column", "cursor_line", "script"]
        );
        let back = EditorSession::from_json(&json).ok();
        assert_eq!(back.as_ref(), Some(&session));
        let kept = session.retain_existing(|p| p.as_path() != Path::new("a.sql"));
        assert_eq!(kept.tabs.len(), 2);
        assert_eq!(kept.active, Some(0), "b.sql is still the active tab");
        let bad_active = r#"{"schema": 1, "tabs": [], "active": 4}"#;
        assert_eq!(
            EditorSession::from_json(bad_active)
                .ok()
                .and_then(|s| s.active),
            None
        );
    }
}
