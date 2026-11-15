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

use std::collections::HashMap;

use crate::error::DbeaverError;
use crate::fsread::{self, FileId, MAX_SMALL_FILE_BYTES, ScanBudget};
use crate::mapping::MAX_DATA_SOURCE_FILES;
use crate::source::CancelFlag;
use crate::source::MAX_SOURCE_PROJECTS;

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
    /// (`.data-sources.json.bak`) and `*.bak` files are never listed. At most 65 are listed,
    /// one more than an import reads, so the import can report that there are more (a
    /// `.dbeaver` listing cut by its own limits always leaves more than 64).
    pub data_source_files: Vec<PathBuf>,
    /// `credentials-config.json`, when present.
    pub credentials_file: Option<PathBuf>,
    /// The DBeaver workspace holding the project (the project folder itself for a picked
    /// project whose parent is not a workspace). Database files under it are imported
    /// read-only (I4).
    pub workspace: PathBuf,
}

/// Most directories one detection ([`detect_workspaces`], [`open_folder`]) lists.
pub const MAX_DETECT_DIRS: usize = 2_048;
/// Most directory entries one detection reads or `stat`s, all listings together.
pub const MAX_DETECT_ENTRIES: usize = 200_000;
/// Most skipped duplicate project folders recorded per workspace; the others are counted.
pub const MAX_LISTED_DUPLICATES: usize = 256;

/// A DBeaver workspace and its projects.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Workspace {
    /// Install kind deduced from the location.
    pub kind: InstallKind,
    /// Workspace folder (or the project folder itself for a single picked project).
    pub path: PathBuf,
    /// `product-version` from `.metadata/dbeaver-workspace.properties`, when readable.
    pub product_version: Option<String>,
    /// Projects with at least one data-sources file, at most [`MAX_SOURCE_PROJECTS`].
    pub projects: Vec<Project>,
    /// Project folders not listed because their `.dbeaver` directory is the same directory
    /// (same device and inode) as a listed project's: a symbolic link, a `..` path. At most
    /// [`MAX_LISTED_DUPLICATES`]; [`Workspace::more_duplicates`] counts the others.
    pub duplicate_projects: Vec<DuplicateProject>,
    /// Duplicate project folders past [`MAX_LISTED_DUPLICATES`], counted only.
    pub more_duplicates: usize,
    /// The listing was cut: the folder may hold more projects than [`Workspace::projects`]
    /// (more than [`MAX_SOURCE_PROJECTS`] candidates, a directory over its entry limits, or the
    /// detection budget [`MAX_DETECT_DIRS`] / [`MAX_DETECT_ENTRIES`] spent). **A caller that
    /// imports the projects must report it**; [`crate::import_source`] does.
    pub listing_truncated: bool,
}

/// A project folder skipped while listing because it is the same folder as a listed project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuplicateProject {
    /// The folder skipped.
    pub path: PathBuf,
    /// The listed project folder it resolves to.
    pub same_as: PathBuf,
}

/// Projects found in one folder.
struct ProjectListing {
    projects: Vec<Project>,
    duplicates: Vec<DuplicateProject>,
    more_duplicates: usize,
    truncated: bool,
}

impl ProjectListing {
    fn into_workspace(
        self,
        kind: InstallKind,
        path: PathBuf,
        product_version: Option<String>,
    ) -> Workspace {
        Workspace {
            kind,
            path,
            product_version,
            projects: self.projects,
            duplicate_projects: self.duplicates,
            more_duplicates: self.more_duplicates,
            listing_truncated: self.truncated,
        }
    }

    fn is_empty(&self) -> bool {
        self.projects.is_empty() && self.duplicates.is_empty() && !self.truncated
    }
}

fn detect_budget(cancel: Option<&CancelFlag>) -> ScanBudget<'_> {
    ScanBudget::new(MAX_DETECT_DIRS, MAX_DETECT_ENTRIES, cancel)
}

/// Workspaces found in the known locations. Locations that do not exist or hold no project are
/// skipped; nothing is created. Not cancellable: see [`detect_workspaces_with`].
#[must_use]
pub fn detect_workspaces(roots: &SearchRoots) -> Vec<Workspace> {
    detect_workspaces_with(roots, &CancelFlag::new()).unwrap_or_default()
}

/// [`detect_workspaces`], stopping when `cancel` is set. All locations share one detection
/// budget ([`MAX_DETECT_DIRS`], [`MAX_DETECT_ENTRIES`]).
///
/// # Errors
/// Only [`DbeaverError::Cancelled`].
pub fn detect_workspaces_with(
    roots: &SearchRoots,
    cancel: &CancelFlag,
) -> Result<Vec<Workspace>, DbeaverError> {
    let mut budget = detect_budget(Some(cancel));
    let mut found = Vec::new();
    for (kind, path) in roots.candidates() {
        budget.check_cancel()?;
        if !fsread::is_dir(&path) {
            continue;
        }
        let listing = match list_projects_in(&path, &mut budget) {
            Ok(l) => l,
            Err(DbeaverError::Cancelled) => return Err(DbeaverError::Cancelled),
            Err(_) => continue,
        };
        tracing::debug!(
            ?kind,
            projects = listing.projects.len(),
            truncated = listing.truncated,
            "DBeaver workspace location checked"
        );
        if listing.is_empty() {
            continue;
        }
        let version = product_version(&path);
        found.push(listing.into_workspace(kind, path, version));
    }
    Ok(found)
}

/// Opens a folder chosen by the user: a workspace (its project sub-folders are listed) or a
/// single project folder (with a `.dbeaver/` directory). Not cancellable: see
/// [`open_folder_with`].
///
/// # Errors
/// [`DbeaverError::Io`] when the folder cannot be listed, [`DbeaverError::NotDbeaver`] when it
/// holds no DBeaver project.
pub fn open_folder(path: &Path) -> Result<Workspace, DbeaverError> {
    open_folder_with(path, &CancelFlag::new())
}

/// [`open_folder`], stopping when `cancel` is set, under one detection budget
/// ([`MAX_DETECT_DIRS`], [`MAX_DETECT_ENTRIES`]). A folder whose listing was cut before any
/// project was found is returned with no project and [`Workspace::listing_truncated`] set, so
/// the cut is reported instead of "not DBeaver".
///
/// # Errors
/// As [`open_folder`], and [`DbeaverError::Cancelled`].
pub fn open_folder_with(path: &Path, cancel: &CancelFlag) -> Result<Workspace, DbeaverError> {
    let mut budget = detect_budget(Some(cancel));
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
    let (single, truncated) = read_project(path, &workspace, &mut budget)?;
    if let Some(project) = single {
        let listing = ProjectListing {
            projects: vec![project],
            duplicates: Vec::new(),
            more_duplicates: 0,
            truncated,
        };
        let version = path.parent().and_then(product_version);
        return Ok(listing.into_workspace(InstallKind::Custom, path.to_path_buf(), version));
    }
    // A listing error (permissions) is propagated instead of reporting "not DBeaver".
    let listing = list_projects_in(path, &mut budget)?;
    if listing.is_empty() {
        return Err(DbeaverError::NotDbeaver {
            path: path.to_path_buf(),
        });
    }
    let version = product_version(path);
    Ok(listing.into_workspace(InstallKind::Custom, path.to_path_buf(), version))
}

/// Projects of a workspace: its non-hidden sub-folders that hold `.dbeaver/data-sources*.json`,
/// in name order, at most [`MAX_SOURCE_PROJECTS`]. Folders whose `.dbeaver` directory is the
/// same as a listed project's (symbolic links) are not listed twice.
///
/// This function does not say whether the listing was cut or what it skipped; the
/// [`Workspace`] values of [`detect_workspaces`] and [`open_folder`] do.
///
/// Projects linked from outside the workspace folder (registered in Eclipse metadata) are not
/// found; the user can pick them with [`open_folder`] (to verify against a real workspace).
#[must_use]
pub fn list_projects(workspace: &Path) -> Vec<Project> {
    let mut budget = detect_budget(None);
    list_projects_in(workspace, &mut budget)
        .map(|l| l.projects)
        .unwrap_or_default()
}

/// Lists the projects of `workspace` under `budget`.
///
/// Each candidate costs one `stat` of its `.dbeaver` directory; a candidate whose `.dbeaver`
/// was already seen (same device and inode) is recorded as a duplicate without being listed.
fn list_projects_in(
    workspace: &Path,
    budget: &mut ScanBudget<'_>,
) -> Result<ProjectListing, DbeaverError> {
    let listing = fsread::list_dir(workspace, |n| !n.starts_with('.'), budget)?;
    let mut out = ProjectListing {
        projects: Vec::new(),
        duplicates: Vec::new(),
        more_duplicates: 0,
        truncated: listing.truncated,
    };
    let mut seen: HashMap<FileId, Option<PathBuf>> = HashMap::new();
    for entry in listing.entries.into_iter().filter(|e| e.is_dir) {
        budget.check_cancel()?;
        let dot = entry.path.join(".dbeaver");
        let Some(id) = fsread::dir_id(&dot, budget) else {
            if budget.exhausted {
                out.truncated = true;
                break;
            }
            continue;
        };
        if let Some(first) = seen.get(&id) {
            // A link to a folder that is not a project: nothing to read, nothing to report.
            let Some(first) = first else { continue };
            if out.duplicates.len() < MAX_LISTED_DUPLICATES {
                out.duplicates.push(DuplicateProject {
                    path: entry.path.clone(),
                    same_as: first.clone(),
                });
            } else {
                out.more_duplicates += 1;
            }
            continue;
        }
        if out.projects.len() >= MAX_SOURCE_PROJECTS {
            // Another candidate with a `.dbeaver` directory: there may be more projects.
            out.truncated = true;
            break;
        }
        let (project, truncated) = read_project(&entry.path, workspace, budget)?;
        out.truncated |= truncated;
        // Recorded even when it is not a project, so links to it are not listed again.
        seen.insert(id, project.as_ref().map(|_| entry.path.clone()));
        if let Some(project) = project {
            out.projects.push(project);
        }
        if budget.exhausted {
            out.truncated = true;
            break;
        }
    }
    Ok(out)
}

/// Reads one project folder, if it holds at least one data-sources file. The flag is `true`
/// when the `.dbeaver` listing was cut.
fn read_project(
    path: &Path,
    workspace: &Path,
    budget: &mut ScanBudget<'_>,
) -> Result<(Option<Project>, bool), DbeaverError> {
    let dot_dbeaver = path.join(".dbeaver");
    if !fsread::is_dir(&dot_dbeaver) {
        return Ok((None, false));
    }
    let listing = match fsread::list_dir(&dot_dbeaver, is_data_sources_name, budget) {
        Ok(l) => l,
        Err(DbeaverError::Cancelled) => return Err(DbeaverError::Cancelled),
        Err(_) => return Ok((None, false)),
    };
    let mut data_source_files: Vec<(bool, String, PathBuf)> = listing
        .entries
        .iter()
        .filter(|e| e.is_file)
        .map(|e| {
            (
                e.name != "data-sources.json",
                e.name.clone(),
                e.path.clone(),
            )
        })
        .collect();
    // A cut listing may have left out the main file (`-` sorts before `.`).
    let main = dot_dbeaver.join("data-sources.json");
    if listing.truncated
        && !data_source_files.iter().any(|(other, _, _)| !other)
        && fsread::is_file(&main)
    {
        data_source_files.push((false, "data-sources.json".to_owned(), main));
    }
    if data_source_files.is_empty() {
        return Ok((None, listing.truncated));
    }
    data_source_files.sort();
    // One more than an import reads, so the import can report that there are more.
    data_source_files.truncate(MAX_DATA_SOURCE_FILES + 1);
    let credentials = dot_dbeaver.join("credentials-config.json");
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let project = Project {
        name,
        path: path.to_path_buf(),
        data_source_files: data_source_files.into_iter().map(|(_, _, p)| p).collect(),
        credentials_file: fsread::is_file(&credentials).then_some(credentials),
        workspace: workspace.to_path_buf(),
    };
    Ok((Some(project), listing.truncated))
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
