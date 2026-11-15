//! Where DBeaver keeps its workspaces, to flag database files that live inside one (I4,
//! ADR-0006; ROADMAP driver-API PR row L2).
//!
//! A file connection (SQLite, DuckDB…) imported from DBeaver often points at a database inside
//! `DBeaverData` (DBeaver's sample database, files users keep next to their scripts). NineSQL
//! never modifies DBeaver files, so the core sets [`ConnectParams::inside_dbeaver_workspace`]
//! for every [`Endpoint::File`](crate::Endpoint::File) under a DBeaver workspace, and file
//! drivers then open it read-only (and, for SQLite, immutable).
//!
//! A path counts as inside when, after resolving symbolic links ([`std::fs::canonicalize`] of
//! the file, or of its nearest existing ancestor when it does not exist yet):
//! - it lies under a **known root**: the `DBeaverData` directory of every known install location
//!   (native, Flatpak, Snap, [`ninesql_dbeaver::detect::SearchRoots::candidates`]), whether it
//!   exists or not, plus every folder added with [`DbeaverRoots::add`] (a workspace or project
//!   the user picked for an import);
//! - or one of its components is named `DBeaverData` (an install location NineSQL does not
//!   know);
//! - or one of its ancestors is **detected** as a DBeaver workspace (it holds
//!   `.metadata/dbeaver-workspace.properties`) or project (it holds a `.dbeaver` directory).
//!
//! Every check is read-only (`stat` and `canonicalize`, I4) and blocking: the session manager
//! runs it on the blocking pool. When in doubt (a check that fails), the file is treated as
//! inside: a read-only open is the safe side.
//!
//! [`ConnectParams::inside_dbeaver_workspace`]: ninesql_driver_api::ConnectParams::inside_dbeaver_workspace

use std::ffi::OsStr;
use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock};

use ninesql_dbeaver::detect::SearchRoots;

/// Name of DBeaver's data directory in every known install location.
const DBEAVER_DATA: &str = "DBeaverData";

/// Most ancestors inspected for workspace / project markers.
const MAX_ANCESTORS: usize = 64;

/// The DBeaver workspace roots of this session. Cheap to clone; clones share the list, so a root
/// added after startup ([`DbeaverRoots::add`]) is seen by the session manager at once.
#[derive(Clone, Default)]
pub struct DbeaverRoots {
    roots: Arc<RwLock<Vec<PathBuf>>>,
}

impl fmt::Debug for DbeaverRoots {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DbeaverRoots")
            .field("roots", &self.roots().len())
            .finish()
    }
}

impl DbeaverRoots {
    /// No known root (the `DBeaverData` component rule and the marker detection still apply).
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// The `DBeaverData` directory of every known install location under `$HOME`; no known
    /// root when `HOME` is unset or relative.
    #[must_use]
    pub fn from_env() -> Self {
        SearchRoots::from_env().map_or_else(Self::none, |r| Self::from_search_roots(&r))
    }

    /// The `DBeaverData` directory of every known install location under `roots.home`.
    #[must_use]
    pub fn from_search_roots(roots: &SearchRoots) -> Self {
        let out = Self::none();
        for (_, workspace) in roots.candidates() {
            // `…/DBeaverData/workspace6`: the whole `DBeaverData` is DBeaver's.
            out.add(
                workspace
                    .parent()
                    .map_or(workspace.clone(), Path::to_path_buf),
            );
        }
        out
    }

    /// Adds a root: a DBeaver workspace or project folder (for example the one the user picked
    /// for an import, ADR-0006 §2). A relative path is ignored.
    pub fn add(&self, root: impl Into<PathBuf>) {
        let root = root.into();
        if !root.is_absolute() {
            tracing::debug!("relative DBeaver root ignored");
            return;
        }
        let mut roots = self.roots.write().unwrap_or_else(PoisonError::into_inner);
        if !roots.contains(&root) {
            roots.push(root);
        }
    }

    /// The known roots, in the order they were added.
    #[must_use]
    pub fn roots(&self) -> Vec<PathBuf> {
        self.roots
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Whether the database file `path` lies inside a DBeaver workspace (see the module
    /// documentation). **Blocking** (file system metadata): run it on a blocking thread.
    #[must_use]
    pub fn contains(&self, path: &Path) -> bool {
        let Ok(lexical) = std::path::absolute(path) else {
            // No current directory to resolve a relative path against: fail closed.
            return true;
        };
        let lexical = normalize(&lexical);
        let resolved = resolve(&lexical);
        for candidate in [&lexical, &resolved] {
            if candidate
                .components()
                .any(|c| c.as_os_str() == OsStr::new(DBEAVER_DATA))
            {
                return true;
            }
        }
        for root in self.roots() {
            let root_lexical = normalize(&root);
            let root_resolved =
                std::fs::canonicalize(&root).unwrap_or_else(|_| root_lexical.clone());
            for candidate in [&lexical, &resolved] {
                if candidate.starts_with(&root_lexical) || candidate.starts_with(&root_resolved) {
                    return true;
                }
            }
        }
        detected_marker(&resolved) || detected_marker(&lexical)
    }
}

/// Removes `.` and resolves `..` lexically (on an absolute path).
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The path with symbolic links resolved: the canonical file, or the canonical nearest existing
/// ancestor followed by the missing components. Falls back to the lexical path.
fn resolve(lexical: &Path) -> PathBuf {
    if let Ok(p) = std::fs::canonicalize(lexical) {
        return p;
    }
    let mut missing = Vec::new();
    let mut current = lexical;
    while let Some(parent) = current.parent() {
        if let Some(name) = current.file_name() {
            missing.push(name.to_owned());
        }
        if let Ok(mut p) = std::fs::canonicalize(parent) {
            for name in missing.iter().rev() {
                p.push(name);
            }
            return p;
        }
        current = parent;
    }
    lexical.to_path_buf()
}

/// Whether an ancestor of `path` (its parent first) is a DBeaver workspace or project folder.
fn detected_marker(path: &Path) -> bool {
    path.ancestors().skip(1).take(MAX_ANCESTORS).any(|dir| {
        dir.join(".metadata/dbeaver-workspace.properties").is_file()
            || dir.join(".dbeaver").is_dir()
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).
mod tests {
    use super::*;

    #[test]
    fn known_install_locations_are_roots_even_when_missing() {
        let home = tempfile::tempdir().unwrap();
        let roots = DbeaverRoots::from_search_roots(&SearchRoots {
            home: home.path().to_path_buf(),
        });
        assert_eq!(roots.roots().len(), 3);
        assert!(roots.roots()[0].ends_with(".local/share/DBeaverData"));
        let inside = home
            .path()
            .join(".var/app/io.dbeaver.DBeaverCommunity/data/DBeaverData/workspace6/General/x.db");
        assert!(roots.contains(&inside));
        let outside = home.path().join("data/app.db");
        assert!(!roots.contains(&outside));
    }

    #[test]
    fn a_symbolic_link_into_a_root_is_inside() {
        let home = tempfile::tempdir().unwrap();
        let ws = home.path().join("custom-ws/General");
        std::fs::create_dir_all(&ws).unwrap();
        let db = ws.join("sample.db");
        std::fs::write(&db, b"").unwrap();
        let roots = DbeaverRoots::none();
        roots.add(home.path().join("custom-ws"));
        let link = home.path().join("link.db");
        std::os::unix::fs::symlink(&db, &link).unwrap();
        assert!(roots.contains(&link));
        // `..` does not escape the check either.
        let dotted = home.path().join("elsewhere/../custom-ws/General/sample.db");
        assert!(roots.contains(&dotted));
        assert!(!roots.contains(&home.path().join("other.db")));
    }

    #[test]
    fn unknown_locations_are_caught_by_name_and_markers() {
        let home = tempfile::tempdir().unwrap();
        let roots = DbeaverRoots::none();
        assert!(roots.contains(&home.path().join("opt/DBeaverData/ws/a.db")));

        let ws = home.path().join("my-workspace");
        std::fs::create_dir_all(ws.join(".metadata")).unwrap();
        std::fs::write(ws.join(".metadata/dbeaver-workspace.properties"), b"").unwrap();
        assert!(roots.contains(&ws.join("Project/db/a.sqlite")));

        let project = home.path().join("picked-project");
        std::fs::create_dir_all(project.join(".dbeaver")).unwrap();
        assert!(roots.contains(&project.join("a.sqlite")));

        assert!(!roots.contains(&home.path().join("plain/a.sqlite")));
    }

    #[test]
    fn relative_roots_are_ignored_and_clones_share_the_list() {
        let roots = DbeaverRoots::none();
        let clone = roots.clone();
        roots.add("relative/ws");
        assert!(clone.roots().is_empty());
        roots.add("/abs/ws");
        roots.add("/abs/ws");
        assert_eq!(clone.roots(), [PathBuf::from("/abs/ws")]);
    }
}
