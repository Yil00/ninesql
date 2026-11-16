//! Locations of NineSQL files under the XDG base directories (ADR-0008 §1).
//!
//! ```text
//! $XDG_CONFIG_HOME/ninesql/settings.toml
//! $XDG_DATA_HOME/ninesql/workspace/<project>/{project.json, connections.json,
//!                                             connection-types.json, scripts/, diagrams/}
//! $XDG_DATA_HOME/ninesql/vault.bin
//! $XDG_DATA_HOME/ninesql/known_hosts
//! $XDG_DATA_HOME/ninesql/ninesql.lock
//! $XDG_STATE_HOME/ninesql/{history.sqlite, ninesql.log}
//! ```

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::StorageError;

/// Application directory name under each XDG base directory.
pub const APP_DIR: &str = "ninesql";

/// The three XDG base directories NineSQL uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XdgDirs {
    /// `$XDG_CONFIG_HOME`, default `$HOME/.config`.
    pub config_home: PathBuf,
    /// `$XDG_DATA_HOME`, default `$HOME/.local/share`.
    pub data_home: PathBuf,
    /// `$XDG_STATE_HOME`, default `$HOME/.local/state`.
    pub state_home: PathBuf,
}

impl XdgDirs {
    /// Reads the process environment.
    ///
    /// # Errors
    /// [`StorageError::NoHome`] when a fallback is needed and `$HOME` is unusable.
    pub fn from_env() -> Result<Self, StorageError> {
        Self::from_lookup(|name| std::env::var_os(name))
    }

    /// Resolves the directories from an environment lookup (testable).
    ///
    /// Per the XDG Base Directory specification, a variable that is unset, empty or holds a
    /// relative path is ignored and the `$HOME`-based default is used.
    ///
    /// # Errors
    /// [`StorageError::NoHome`] when a fallback is needed and `$HOME` is unset or relative.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<OsString>) -> Result<Self, StorageError> {
        let absolute = |name: &str| lookup(name).map(PathBuf::from).filter(|p| p.is_absolute());
        let home = absolute("HOME");
        let resolve = |var: &str, default: &[&str]| -> Result<PathBuf, StorageError> {
            if let Some(dir) = absolute(var) {
                return Ok(dir);
            }
            let mut dir = home.clone().ok_or(StorageError::NoHome)?;
            dir.extend(default);
            Ok(dir)
        };
        Ok(Self {
            config_home: resolve("XDG_CONFIG_HOME", &[".config"])?,
            data_home: resolve("XDG_DATA_HOME", &[".local", "share"])?,
            state_home: resolve("XDG_STATE_HOME", &[".local", "state"])?,
        })
    }
}

/// Every NineSQL location, derived from [`XdgDirs`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    config_dir: PathBuf,
    data_dir: PathBuf,
    state_dir: PathBuf,
}

impl Paths {
    /// NineSQL directories under the given XDG base directories.
    #[must_use]
    pub fn new(xdg: &XdgDirs) -> Self {
        Self {
            config_dir: xdg.config_home.join(APP_DIR),
            data_dir: xdg.data_home.join(APP_DIR),
            state_dir: xdg.state_home.join(APP_DIR),
        }
    }

    /// NineSQL directories from the process environment.
    ///
    /// # Errors
    /// See [`XdgDirs::from_env`].
    pub fn from_env() -> Result<Self, StorageError> {
        Ok(Self::new(&XdgDirs::from_env()?))
    }

    /// `$XDG_CONFIG_HOME/ninesql`.
    #[must_use]
    pub fn config_dir(&self) -> &Path {
        &self.config_dir
    }

    /// `$XDG_DATA_HOME/ninesql`.
    #[must_use]
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// `$XDG_STATE_HOME/ninesql`.
    #[must_use]
    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    /// `$XDG_CONFIG_HOME/ninesql/settings.toml`.
    ///
    /// Not covered by the workspace lock ([`Self::lock_file`]): the settings writer serializes
    /// its writes within one process only. With two NineSQL processes, the last write wins; each
    /// write is atomic (no torn file) and the file never holds a secret.
    #[must_use]
    pub fn settings_file(&self) -> PathBuf {
        self.config_dir.join("settings.toml")
    }

    /// `$XDG_DATA_HOME/ninesql/workspace`.
    #[must_use]
    pub fn workspace_dir(&self) -> PathBuf {
        self.data_dir.join("workspace")
    }

    /// Paths of one project of the workspace.
    #[must_use]
    pub fn project(&self, name: &ProjectName) -> ProjectPaths {
        ProjectPaths {
            dir: self.workspace_dir().join(name.as_str()),
        }
    }

    /// `$XDG_DATA_HOME/ninesql/vault.bin` (ADR-0005 §2).
    #[must_use]
    pub fn vault_file(&self) -> PathBuf {
        self.data_dir.join("vault.bin")
    }

    /// `$XDG_DATA_HOME/ninesql/known_hosts`: NineSQL's own SSH host keys (ADR-0011 §6).
    #[must_use]
    pub fn known_hosts_file(&self) -> PathBuf {
        self.data_dir.join("known_hosts")
    }

    /// `$XDG_DATA_HOME/ninesql/ninesql.lock`: the advisory lock held while a NineSQL process uses
    /// the workspace ([`crate::lock`]).
    #[must_use]
    pub fn lock_file(&self) -> PathBuf {
        self.data_dir.join("ninesql.lock")
    }

    /// `$XDG_STATE_HOME/ninesql/history.sqlite`.
    #[must_use]
    pub fn history_file(&self) -> PathBuf {
        self.state_dir.join("history.sqlite")
    }

    /// `$XDG_STATE_HOME/ninesql/ninesql.log`.
    #[must_use]
    pub fn log_file(&self) -> PathBuf {
        self.state_dir.join("ninesql.log")
    }
}

/// Files and directories of one workspace project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectPaths {
    dir: PathBuf,
}

impl ProjectPaths {
    /// The project directory.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// `project.json`.
    #[must_use]
    pub fn project_file(&self) -> PathBuf {
        self.dir.join("project.json")
    }

    /// `connections.json`.
    #[must_use]
    pub fn connections_file(&self) -> PathBuf {
        self.dir.join("connections.json")
    }

    /// `connection-types.json`.
    #[must_use]
    pub fn connection_types_file(&self) -> PathBuf {
        self.dir.join("connection-types.json")
    }

    /// `navigator.json`: navigator filters per connection (P4-B, F-NAV-04).
    #[must_use]
    pub fn navigator_file(&self) -> PathBuf {
        self.dir.join("navigator.json")
    }

    /// `scripts/`.
    #[must_use]
    pub fn scripts_dir(&self) -> PathBuf {
        self.dir.join("scripts")
    }

    /// `diagrams/`.
    #[must_use]
    pub fn diagrams_dir(&self) -> PathBuf {
        self.dir.join("diagrams")
    }
}

/// Maximum length of a project name, in bytes.
pub const MAX_PROJECT_NAME_LEN: usize = 128;

/// Name of a workspace project, safe to use as a single directory name.
///
/// Rejects anything that could escape the workspace directory or surprise a file manager:
/// empty, `/`, `\`, NUL and other control characters, a leading `.` (hidden, `..`), a trailing
/// space or dot, more than [`MAX_PROJECT_NAME_LEN`] bytes.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ProjectName(String);

impl ProjectName {
    /// Name of the project created on first start.
    pub const DEFAULT: &'static str = "General";

    /// Validates a project name.
    ///
    /// # Errors
    /// [`StorageError::InvalidProjectName`].
    pub fn new(name: impl Into<String>) -> Result<Self, StorageError> {
        let name = name.into();
        let valid = !name.is_empty()
            && name.len() <= MAX_PROJECT_NAME_LEN
            && !name.starts_with('.')
            && !name.ends_with([' ', '.'])
            && !name
                .chars()
                .any(|c| c == '/' || c == '\\' || c.is_control());
        if valid {
            Ok(Self(name))
        } else {
            Err(StorageError::InvalidProjectName)
        }
    }

    /// The default project (`General`).
    #[must_use]
    pub fn default_project() -> Self {
        Self(Self::DEFAULT.to_owned())
    }

    /// The name as a string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProjectName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for ProjectName {
    type Error = StorageError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<ProjectName> for String {
    fn from(value: ProjectName) -> Self {
        value.0
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).
mod tests {
    use std::collections::HashMap;
    use std::path::Component;

    use proptest::prelude::*;

    use super::*;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let map: HashMap<String, OsString> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), OsString::from(v)))
            .collect();
        move |k| map.get(k).cloned()
    }

    #[test]
    fn defaults_from_home() {
        let xdg = XdgDirs::from_lookup(env(&[("HOME", "/home/u")])).unwrap();
        assert_eq!(xdg.config_home, Path::new("/home/u/.config"));
        assert_eq!(xdg.data_home, Path::new("/home/u/.local/share"));
        assert_eq!(xdg.state_home, Path::new("/home/u/.local/state"));
    }

    #[test]
    fn explicit_absolute_variables_win() {
        let xdg = XdgDirs::from_lookup(env(&[
            ("HOME", "/home/u"),
            ("XDG_CONFIG_HOME", "/c"),
            ("XDG_DATA_HOME", "/d"),
            ("XDG_STATE_HOME", "/s"),
        ]))
        .unwrap();
        let p = Paths::new(&xdg);
        assert_eq!(p.settings_file(), Path::new("/c/ninesql/settings.toml"));
        assert_eq!(p.workspace_dir(), Path::new("/d/ninesql/workspace"));
        assert_eq!(p.vault_file(), Path::new("/d/ninesql/vault.bin"));
        assert_eq!(p.known_hosts_file(), Path::new("/d/ninesql/known_hosts"));
        assert_eq!(p.lock_file(), Path::new("/d/ninesql/ninesql.lock"));
        assert_eq!(p.history_file(), Path::new("/s/ninesql/history.sqlite"));
        assert_eq!(p.log_file(), Path::new("/s/ninesql/ninesql.log"));
        let proj = p.project(&ProjectName::default_project());
        assert_eq!(
            proj.connections_file(),
            Path::new("/d/ninesql/workspace/General/connections.json")
        );
        assert_eq!(
            proj.connection_types_file(),
            Path::new("/d/ninesql/workspace/General/connection-types.json")
        );
        assert_eq!(
            proj.project_file(),
            Path::new("/d/ninesql/workspace/General/project.json")
        );
        assert_eq!(
            proj.navigator_file(),
            Path::new("/d/ninesql/workspace/General/navigator.json")
        );
        assert_eq!(
            proj.scripts_dir(),
            Path::new("/d/ninesql/workspace/General/scripts")
        );
        assert_eq!(
            proj.diagrams_dir(),
            Path::new("/d/ninesql/workspace/General/diagrams")
        );
    }

    #[test]
    fn relative_or_empty_variables_are_ignored() {
        let xdg = XdgDirs::from_lookup(env(&[
            ("HOME", "/home/u"),
            ("XDG_CONFIG_HOME", "relative/conf"),
            ("XDG_DATA_HOME", ""),
        ]))
        .unwrap();
        assert_eq!(xdg.config_home, Path::new("/home/u/.config"));
        assert_eq!(xdg.data_home, Path::new("/home/u/.local/share"));
    }

    #[test]
    fn missing_or_relative_home_is_an_error_only_when_needed() {
        assert!(matches!(
            XdgDirs::from_lookup(env(&[])),
            Err(StorageError::NoHome)
        ));
        assert!(matches!(
            XdgDirs::from_lookup(env(&[("HOME", "home/u")])),
            Err(StorageError::NoHome)
        ));
        assert!(
            XdgDirs::from_lookup(env(&[
                ("XDG_CONFIG_HOME", "/c"),
                ("XDG_DATA_HOME", "/d"),
                ("XDG_STATE_HOME", "/s"),
            ]))
            .is_ok()
        );
    }

    #[test]
    fn project_name_rules() {
        for ok in ["General", "Client A", "prod-2026", "données", "a.b"] {
            assert!(ProjectName::new(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            ".",
            "..",
            ".hidden",
            "a/b",
            "a\\b",
            "a\0b",
            "a\nb",
            "trailing ",
            "trailing.",
        ] {
            assert!(ProjectName::new(bad).is_err(), "{bad:?}");
        }
        assert!(ProjectName::new("x".repeat(MAX_PROJECT_NAME_LEN)).is_ok());
        assert!(ProjectName::new("x".repeat(MAX_PROJECT_NAME_LEN + 1)).is_err());
    }

    proptest! {
        /// Any accepted project name maps to exactly one normal path component directly under
        /// the workspace directory: no traversal is possible.
        #[test]
        fn accepted_project_names_stay_inside_the_workspace(name in "\\PC{1,40}") {
            if let Ok(project) = ProjectName::new(name) {
                let paths = Paths::new(&XdgDirs {
                    config_home: "/c".into(),
                    data_home: "/d".into(),
                    state_home: "/s".into(),
                });
                let dir = paths.project(&project).dir().to_path_buf();
                let rel = dir.strip_prefix(paths.workspace_dir()).unwrap();
                let comps: Vec<_> = rel.components().collect();
                prop_assert_eq!(comps.len(), 1);
                prop_assert!(matches!(comps[0], Component::Normal(_)));
                prop_assert_eq!(rel.as_os_str(), project.as_str());
            }
        }

        #[test]
        fn project_name_serde_round_trip(name in "[A-Za-z0-9][A-Za-z0-9 _-]{0,30}[A-Za-z0-9]") {
            let project = ProjectName::new(name).unwrap();
            let json = serde_json::to_string(&project).unwrap();
            let back: ProjectName = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(back, project);
        }
    }
}
