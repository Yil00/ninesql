//! Writes a generated export to a **new** file or project folder the user chose (I4: never into
//! an existing DBeaver workspace, never over an existing file).
//!
//! This is the only code of the crate that creates files. It never opens an existing file for
//! writing, never renames over a path, and never deletes anything but the temporary file and
//! the folders it created itself in the same call:
//! - a new file is first written to a temporary file in the same directory (created with
//!   `O_CREAT | O_EXCL`, mode `0600`, which never follows a symbolic link), synced, then
//!   **hard-linked** to its final name, which fails if that name exists (an atomic
//!   create-if-absent, unlike `rename`); on a filesystem without hard links the final file is
//!   created directly with `O_CREAT | O_EXCL` instead (never overwriting, but a crash can leave
//!   it partial);
//! - a new project folder is created with `mkdir` (fails if it exists), then
//!   `.dbeaver/data-sources.json` and an Eclipse `.project` file inside it;
//! - the target's directory is resolved (`canonicalize`) and refused when it lies inside a
//!   DBeaver workspace or data directory, by this module's own check and by the caller's
//!   (`ninesql-core`'s `DbeaverRoots`, which also knows the folders picked for imports).
//!
//! Residual risk: the directory is checked, then used by path. Someone who can replace a
//! directory of the path between the check and the creation (another local process of the same
//! user) could redirect the new file; it still never overwrites an existing file.

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use crate::detect::SearchRoots;

use super::Export;

/// Where to write an export. Both forms create new paths only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExportTarget {
    /// A new file, such as `~/exports/data-sources-ninesql.json`. Its directory must exist; the
    /// file must not. A DBeaver user can copy it into a project's `.dbeaver/` folder while
    /// DBeaver is closed (docs/06 "Export to DBeaver").
    File(PathBuf),
    /// A new DBeaver project folder: `<folder>/.dbeaver/data-sources.json` and
    /// `<folder>/.project`. The folder must not exist; its parent must.
    ProjectFolder(PathBuf),
}

/// What [`write_export`] created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrittenExport {
    /// The `data-sources.json` written.
    pub data_sources: PathBuf,
    /// Every path created, in creation order.
    pub created: Vec<PathBuf>,
}

/// Why an export could not be written. Names the path the user chose, never file content.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ExportWriteError {
    /// The target path is relative.
    #[error("{path}: the export target must be an absolute path")]
    NotAbsolute {
        /// Path concerned.
        path: PathBuf,
    },
    /// The target has no usable file name (`/`, `..`, not UTF-8, a reserved DBeaver name).
    #[error("{path}: not a valid name for an export")]
    InvalidName {
        /// Path concerned.
        path: PathBuf,
    },
    /// The directory that should hold the target does not exist or is not a directory.
    #[error("{path}: the directory does not exist")]
    NoDirectory {
        /// Path concerned.
        path: PathBuf,
    },
    /// The target exists: NineSQL never overwrites a file or reuses a folder.
    #[error("{path} already exists: choose a new file or folder")]
    AlreadyExists {
        /// Path concerned.
        path: PathBuf,
    },
    /// The target lies inside a DBeaver workspace or data directory (I4).
    #[error(
        "{path} is inside a DBeaver workspace: NineSQL never writes there; choose another folder"
    )]
    InsideDbeaver {
        /// Path concerned.
        path: PathBuf,
    },
    /// An I/O error.
    #[error("cannot write {path}: {kind}")]
    Io {
        /// Path concerned.
        path: PathBuf,
        /// I/O error kind.
        kind: io::ErrorKind,
    },
}

/// Writes `export` to `target`, creating new paths only. `is_protected` is the caller's check
/// of DBeaver locations (`ninesql-core` passes `DbeaverRoots::contains`); it is asked about the
/// resolved target and its directory, on top of this crate's own check. **Blocking**.
///
/// # Errors
/// [`ExportWriteError`]; nothing is left behind on failure (the temporary file and any folder
/// created by this call are removed).
pub fn write_export(
    export: &Export,
    target: &ExportTarget,
    is_protected: &dyn Fn(&Path) -> bool,
) -> Result<WrittenExport, ExportWriteError> {
    let home = SearchRoots::from_env().map(|r| r.home);
    write_export_with_home(export, target, is_protected, home.as_deref())
}

/// [`write_export`] with an explicit home directory (the marker walk stops before it).
pub(crate) fn write_export_with_home(
    export: &Export,
    target: &ExportTarget,
    is_protected: &dyn Fn(&Path) -> bool,
    home: Option<&Path>,
) -> Result<WrittenExport, ExportWriteError> {
    let path = match target {
        ExportTarget::File(p) | ExportTarget::ProjectFolder(p) => p,
    };
    if !path.is_absolute() {
        return Err(ExportWriteError::NotAbsolute { path: path.clone() });
    }
    let invalid = || ExportWriteError::InvalidName { path: path.clone() };
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| !n.is_empty() && *n != "." && *n != "..")
        .ok_or_else(invalid)?;
    if name.chars().any(char::is_control) || name.eq_ignore_ascii_case("credentials-config.json") {
        return Err(invalid());
    }
    let parent = path.parent().ok_or_else(invalid)?;
    let parent = fs::canonicalize(parent)
        .ok()
        .filter(|p| p.is_dir())
        .ok_or_else(|| ExportWriteError::NoDirectory {
            path: parent.to_path_buf(),
        })?;
    let resolved = parent.join(name);
    if fs::symlink_metadata(&resolved).is_ok() {
        return Err(ExportWriteError::AlreadyExists { path: resolved });
    }
    let home = home.map(|h| fs::canonicalize(h).unwrap_or_else(|_| h.to_path_buf()));
    if inside_dbeaver(&parent, home.as_deref()) || is_protected(&parent) || is_protected(&resolved)
    {
        return Err(ExportWriteError::InsideDbeaver { path: resolved });
    }
    let mut created = Vec::new();
    let result = match target {
        ExportTarget::File(_) => {
            write_new_file(&parent, name, &export.json, &mut created).map(|p| WrittenExport {
                data_sources: p,
                created: Vec::new(),
            })
        }
        ExportTarget::ProjectFolder(_) => {
            write_project(&resolved, name, export, &mut created).map(|p| WrittenExport {
                data_sources: p,
                created: Vec::new(),
            })
        }
    };
    match result {
        Ok(mut written) => {
            written.created = created;
            tracing::info!(
                connections = export.report.exported_count(),
                "DBeaver export written"
            );
            Ok(written)
        }
        Err(e) => {
            // Undo only what this call created, newest first.
            for p in created.iter().rev() {
                let _ = fs::remove_file(p).or_else(|_| fs::remove_dir(p));
            }
            Err(e)
        }
    }
}

fn io_err(path: &Path, e: &io::Error) -> ExportWriteError {
    if e.kind() == io::ErrorKind::AlreadyExists {
        ExportWriteError::AlreadyExists {
            path: path.to_path_buf(),
        }
    } else {
        ExportWriteError::Io {
            path: path.to_path_buf(),
            kind: e.kind(),
        }
    }
}

/// Creates `path` with `O_CREAT | O_EXCL` (never follows a symbolic link, never truncates).
fn create_new(path: &Path) -> io::Result<fs::File> {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

fn write_all_synced(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut f = create_new(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

/// Writes `bytes` to the new file `dir/name` (see the module documentation).
fn write_new_file(
    dir: &Path,
    name: &str,
    bytes: &[u8],
    created: &mut Vec<PathBuf>,
) -> Result<PathBuf, ExportWriteError> {
    let target = dir.join(name);
    let nonce = {
        use std::hash::{BuildHasher, Hasher};
        let mut h = std::hash::RandomState::new().build_hasher();
        h.write(name.as_bytes());
        h.finish()
    };
    let tmp = dir.join(format!(".{name}.ninesql-export-{nonce:016x}.tmp"));
    if let Err(e) = write_all_synced(&tmp, bytes) {
        if e.kind() != io::ErrorKind::AlreadyExists {
            let _ = fs::remove_file(&tmp);
        }
        return Err(io_err(&tmp, &e));
    }
    let linked = fs::hard_link(&tmp, &target);
    let _ = fs::remove_file(&tmp);
    match linked {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            return Err(io_err(&target, &e));
        }
        Err(_) => {
            // No hard links on this filesystem: create the file directly, still exclusively.
            match create_new(&target) {
                Ok(mut f) => {
                    created.push(target.clone());
                    f.write_all(bytes)
                        .and_then(|()| f.sync_all())
                        .map_err(|e| io_err(&target, &e))?;
                }
                Err(e) => return Err(io_err(&target, &e)),
            }
            sync_dir(dir);
            return Ok(target);
        }
    }
    created.push(target.clone());
    sync_dir(dir);
    Ok(target)
}

fn sync_dir(dir: &Path) {
    if let Ok(d) = fs::File::open(dir) {
        let _ = d.sync_all();
    }
}

fn create_dir(path: &Path, created: &mut Vec<PathBuf>) -> Result<(), ExportWriteError> {
    fs::DirBuilder::new()
        .mode(0o700)
        .create(path)
        .map_err(|e| io_err(path, &e))?;
    created.push(path.to_path_buf());
    Ok(())
}

fn xml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

/// The Eclipse project file DBeaver writes for a project (P0-E fixtures).
fn project_file(name: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<projectDescription>\n\t<name>{}</name>\n\t\
         <comment>Exported from NineSQL</comment>\n\t<projects>\n\t</projects>\n\t<buildSpec>\n\t\
         </buildSpec>\n\t<natures>\n\t\t<nature>org.jkiss.dbeaver.DBeaverNature</nature>\n\t\
         </natures>\n</projectDescription>\n",
        xml_escape(name)
    )
}

fn write_project(
    folder: &Path,
    name: &str,
    export: &Export,
    created: &mut Vec<PathBuf>,
) -> Result<PathBuf, ExportWriteError> {
    create_dir(folder, created)?;
    let dot = folder.join(".dbeaver");
    create_dir(&dot, created)?;
    let ds = write_new_file(&dot, "data-sources.json", &export.json, created)?;
    write_new_file(folder, ".project", project_file(name).as_bytes(), created)?;
    Ok(ds)
}

/// Most ancestors inspected for DBeaver markers.
const MAX_ANCESTORS: usize = 64;
/// Most entries of a `.dbeaver` directory listed; a larger one counts as a project.
const MAX_DBEAVER_ENTRIES: usize = 4096;

/// Whether `dir` (canonical) lies inside a DBeaver location: a `DBeaverData` component, a known
/// install location, or an ancestor that is a DBeaver workspace (`.metadata/
/// dbeaver-workspace.properties`) or project (a `.dbeaver` directory holding a DBeaver project
/// file). The walk stops before `home`, its ancestors and `/` (a stray `~/.dbeaver` does not
/// block every export). **Fail closed**: a marker whose presence cannot be checked counts.
/// The same rules as `ninesql-core`'s `DbeaverRoots`, which the caller also applies.
pub(crate) fn inside_dbeaver(dir: &Path, home: Option<&Path>) -> bool {
    if dir.components().any(|c| c.as_os_str() == "DBeaverData") {
        return true;
    }
    if let Some(home) = home {
        let roots = SearchRoots {
            home: home.to_path_buf(),
        };
        for (_, ws) in roots.candidates() {
            let data = ws.parent().map_or(ws.clone(), Path::to_path_buf);
            if dir.starts_with(&data) {
                return true;
            }
        }
    }
    for ancestor in dir.ancestors().take(MAX_ANCESTORS) {
        if ancestor.parent().is_none() || home.is_some_and(|h| h.starts_with(ancestor)) {
            break;
        }
        match marker_exists(&ancestor.join(".metadata/dbeaver-workspace.properties")) {
            Some(false) => {}
            _ => return true,
        }
        let dot = ancestor.join(".dbeaver");
        match fs::symlink_metadata(&dot) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(_) => return true,
            Ok(m) if !m.is_dir() && !m.file_type().is_symlink() => {}
            Ok(_) => {
                if dbeaver_dir_has_project_file(&dot) {
                    return true;
                }
            }
        }
    }
    false
}

/// `Some(exists)`, or `None` when it cannot be told.
fn marker_exists(path: &Path) -> Option<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Some(true),
        Err(e)
            if e.kind() == io::ErrorKind::NotFound || e.kind() == io::ErrorKind::NotADirectory =>
        {
            Some(false)
        }
        Err(_) => None,
    }
}

fn dbeaver_dir_has_project_file(dot: &Path) -> bool {
    let Ok(entries) = fs::read_dir(dot) else {
        return true;
    };
    for (i, entry) in entries.enumerate() {
        if i >= MAX_DBEAVER_ENTRIES {
            return true;
        }
        let Ok(entry) = entry else {
            return true;
        };
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if (name.starts_with("data-sources") && name.ends_with(".json"))
            || name == "project-settings.json"
            || name == "credentials-config.json"
        {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_names_are_escaped() {
        assert!(project_file("a<b>&\"'").contains("<name>a&lt;b&gt;&amp;&quot;&apos;</name>"));
    }
}
