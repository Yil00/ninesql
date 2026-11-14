//! Finding DBeaver workspaces and their projects (ADR-0006 §2). Read-only: only `stat`,
//! `readdir` and read-only opens (I4).
//!
//! Known locations on Linux (docs/06-dbeaver-interop.md):
//!
//! | Install | Workspace | Status |
//! |---|---|---|
//! | `.deb` / tarball | `~/.local/share/DBeaverData/workspace6` | verified (P0-E fixtures keep this layout) |
//! | Flatpak `io.dbeaver.DBeaverCommunity` | `~/.var/app/io.dbeaver.DBeaverCommunity/data/DBeaverData/workspace6` | to verify |
//! | Snap `dbeaver-ce` | `~/snap/dbeaver-ce/current/.local/share/DBeaverData/workspace6` | to verify |
//!
//! Anything else is a custom folder picked by the user ([`open_folder`]): a workspace (folder of
//! projects) or a single project (a folder with a `.dbeaver/` directory).

use std::path::{Path, PathBuf};

use crate::error::DbeaverError;
use crate::fsread::{self, MAX_SMALL_FILE_BYTES};

/// How DBeaver was installed, deduced from the workspace location.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum InstallKind {
    /// `.deb` package or tarball: `~/.local/share/DBeaverData/workspace6`.
    Native,
    /// Flatpak `io.dbeaver.DBeaverCommunity`.
    Flatpak,
    /// Snap `dbeaver-ce`.
    Snap,
    /// A folder chosen by the user.
    Custom,
}

/// Where to look for DBeaver workspaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchRoots {
    /// The user's home directory.
    pub home: PathBuf,
}

impl SearchRoots {
    /// Roots from the environment (`HOME`). `None` when `HOME` is unset or not absolute.
    #[must_use]
    pub fn from_env() -> Option<Self> {
        let home = PathBuf::from(std::env::var_os("HOME")?);
        home.is_absolute().then_some(Self { home })
    }

    /// The candidate workspace directories, in search order. They may not exist.
    #[must_use]
    pub fn candidates(&self) -> Vec<(InstallKind, PathBuf)> {
        let h = &self.home;
        vec![
            (
                InstallKind::Native,
                h.join(".local/share/DBeaverData/workspace6"),
            ),
            (
                InstallKind::Flatpak,
                h.join(".var/app/io.dbeaver.DBeaverCommunity/data/DBeaverData/workspace6"),
            ),
            (
                InstallKind::Snap,
                h.join("snap/dbeaver-ce/current/.local/share/DBeaverData/workspace6"),
            ),
        ]
    }
}

/// A DBeaver project: a folder holding `.dbeaver/data-sources*.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    /// Project name (the folder name, `General` by default).
    pub name: String,
    /// Project folder.
    pub path: PathBuf,
    /// `data-sources.json` first, then the other `data-sources*.json`, by name. Dot-files
    /// (`.data-sources.json.bak`) and `*.bak` files are never listed.
    pub data_source_files: Vec<PathBuf>,
    /// `credentials-config.json`, when present.
    pub credentials_file: Option<PathBuf>,
    /// The DBeaver workspace holding the project (the project folder itself for a picked
    /// project whose parent is not a workspace). Database files under it are imported
    /// read-only (I4).
    pub workspace: PathBuf,
}

/// A DBeaver workspace and its projects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    /// Install kind deduced from the location.
    pub kind: InstallKind,
    /// Workspace folder (or the project folder itself for a single picked project).
    pub path: PathBuf,
    /// `product-version` from `.metadata/dbeaver-workspace.properties`, when readable.
    pub product_version: Option<String>,
    /// Projects with at least one data-sources file.
    pub projects: Vec<Project>,
}

/// Workspaces found in the known locations. Locations that do not exist or hold no project are
/// skipped; nothing is created.
#[must_use]
pub fn detect_workspaces(roots: &SearchRoots) -> Vec<Workspace> {
    let mut found = Vec::new();
    for (kind, path) in roots.candidates() {
        if !fsread::is_dir(&path) {
            continue;
        }
        let projects = list_projects(&path);
        tracing::debug!(
            ?kind,
            projects = projects.len(),
            "DBeaver workspace location checked"
        );
        if projects.is_empty() {
            continue;
        }
        found.push(Workspace {
            kind,
            product_version: product_version(&path),
            path,
            projects,
        });
    }
    found
}

/// Opens a folder chosen by the user: a workspace (its project sub-folders are listed) or a
/// single project folder (with a `.dbeaver/` directory).
///
/// # Errors
/// [`DbeaverError::Io`] when the folder cannot be listed, [`DbeaverError::NotDbeaver`] when it
/// holds no DBeaver project.
pub fn open_folder(path: &Path) -> Result<Workspace, DbeaverError> {
    if !fsread::is_dir(path) {
        return Err(DbeaverError::NotDbeaver {
            path: path.to_path_buf(),
        });
    }
    let parent_is_workspace = path
        .parent()
        .is_some_and(|p| fsread::is_dir(&p.join(".metadata")));
    let workspace = match path.parent() {
        Some(parent) if parent_is_workspace => parent.to_path_buf(),
        _ => path.to_path_buf(),
    };
    if let Some(project) = read_project(path, &workspace) {
        return Ok(Workspace {
            kind: InstallKind::Custom,
            path: path.to_path_buf(),
            product_version: path.parent().and_then(product_version),
            projects: vec![project],
        });
    }
    // Propagate a listing error (permissions) instead of reporting "not DBeaver".
    fsread::list_dir(path)?;
    let projects = list_projects(path);
    if projects.is_empty() {
        return Err(DbeaverError::NotDbeaver {
            path: path.to_path_buf(),
        });
    }
    Ok(Workspace {
        kind: InstallKind::Custom,
        product_version: product_version(path),
        path: path.to_path_buf(),
        projects,
    })
}

/// Projects of a workspace: its non-hidden sub-folders that hold `.dbeaver/data-sources*.json`.
///
/// Projects linked from outside the workspace folder (registered in Eclipse metadata) are not
/// found; the user can pick them with [`open_folder`] (to verify against a real workspace).
#[must_use]
pub fn list_projects(workspace: &Path) -> Vec<Project> {
    let Ok(entries) = fsread::list_dir(workspace) else {
        return Vec::new();
    };
    entries
        .into_iter()
        .filter(|e| e.is_dir && !e.name.starts_with('.'))
        .filter_map(|e| read_project(&e.path, workspace))
        .collect()
}

/// Reads one project folder, if it holds at least one data-sources file.
fn read_project(path: &Path, workspace: &Path) -> Option<Project> {
    let dot_dbeaver = path.join(".dbeaver");
    if !fsread::is_dir(&dot_dbeaver) {
        return None;
    }
    let entries = fsread::list_dir(&dot_dbeaver).ok()?;
    let mut data_source_files: Vec<(bool, String, PathBuf)> = entries
        .iter()
        .filter(|e| e.is_file && is_data_sources_name(&e.name))
        .map(|e| {
            (
                e.name != "data-sources.json",
                e.name.clone(),
                e.path.clone(),
            )
        })
        .collect();
    if data_source_files.is_empty() {
        return None;
    }
    data_source_files.sort();
    let credentials = dot_dbeaver.join("credentials-config.json");
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    Some(Project {
        name,
        path: path.to_path_buf(),
        data_source_files: data_source_files.into_iter().map(|(_, _, p)| p).collect(),
        credentials_file: fsread::is_file(&credentials).then_some(credentials),
        workspace: workspace.to_path_buf(),
    })
}

/// `data-sources.json` or `data-sources*.json`; never a dot-file or a backup.
pub(crate) fn is_data_sources_name(name: &str) -> bool {
    !name.starts_with('.')
        && name.starts_with("data-sources")
        && name.ends_with(".json")
        && !name.contains(".bak")
}

/// `product-version` of `.metadata/dbeaver-workspace.properties`, when it looks like a version.
fn product_version(workspace: &Path) -> Option<String> {
    let path = workspace
        .join(".metadata")
        .join("dbeaver-workspace.properties");
    if !fsread::is_file(&path) {
        return None;
    }
    let bytes = fsread::read_limited(&path, MAX_SMALL_FILE_BYTES).ok()?;
    let text = std::str::from_utf8(&bytes).ok()?;
    text.lines()
        .filter_map(|l| l.trim().strip_prefix("product-version="))
        .map(str::trim)
        .find(|v| {
            !v.is_empty()
                && v.len() <= 64
                && v.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
        })
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_sources_names() {
        assert!(is_data_sources_name("data-sources.json"));
        assert!(is_data_sources_name("data-sources-2.json"));
        assert!(!is_data_sources_name(".data-sources.json.bak"));
        assert!(!is_data_sources_name("data-sources.json.bak"));
        assert!(!is_data_sources_name("data-sources.bak.json"));
        assert!(!is_data_sources_name("credentials-config.json"));
        assert!(!is_data_sources_name("data-sources.json~"));
    }

    #[test]
    fn candidate_locations() {
        let roots = SearchRoots {
            home: PathBuf::from("/home/nine_user"),
        };
        let c = roots.candidates();
        assert_eq!(c.len(), 3);
        assert_eq!(
            c[0].1,
            PathBuf::from("/home/nine_user/.local/share/DBeaverData/workspace6")
        );
        assert_eq!(c[1].0, InstallKind::Flatpak);
        assert!(
            c[1].1
                .starts_with("/home/nine_user/.var/app/io.dbeaver.DBeaverCommunity")
        );
        assert_eq!(c[2].0, InstallKind::Snap);
        assert!(c[2].1.starts_with("/home/nine_user/snap/dbeaver-ce"));
    }
}
