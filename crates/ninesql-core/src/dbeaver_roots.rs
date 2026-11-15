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
//! the file, or of its nearest existing ancestor followed by the missing names when it does not
//! exist yet):
//! - it lies under a **known root**: the `DBeaverData` directory of every known install location
//!   (native, Flatpak, Snap, [`ninesql_dbeaver::detect::SearchRoots::candidates`]), whether it
//!   exists or not, plus every folder added with [`DbeaverRoots::add`] (a workspace or project
//!   the user picked for an import);
//! - or one of its components is named `DBeaverData` (an install location NineSQL does not
//!   know);
//! - or one of its ancestors is **detected** as a DBeaver workspace (it holds
//!   `.metadata/dbeaver-workspace.properties`) or project (it holds a `.dbeaver` directory with
//!   a DBeaver project file: `data-sources*.json`, `project-settings.json` or
//!   `credentials-config.json`). The ancestor walk stops before the home directory: `$HOME`,
//!   its ancestors and `/` are never taken for a workspace or project. A stray `~/.dbeaver`
//!   never makes home files read-only; a stray `/tmp/.dbeaver` affects nothing only while it
//!   holds no project file. Another local user can still create one with a project file in a
//!   shared directory such as `/tmp` and so get files below it opened read-only: that fails
//!   safe (nothing is written) and is visible through `SessionInfo::inside_dbeaver_workspace`.
//!
//! Every check is read-only (`stat`, `canonicalize`, `read_dir`, I4) and blocking: the session
//! manager runs it on the blocking pool. **Fail closed**: a path that cannot be resolved (any
//! error other than "not found"), a marker whose presence cannot be checked, or a `.dbeaver`
//! directory that cannot be listed makes the file count as inside: a read-only open is the safe
//! side. The canonical path that was checked ([`FileCheck::path`]) is the one handed to the
//! driver, so the check and the open name the same file.
//!
//! [`ConnectParams::inside_dbeaver_workspace`]: ninesql_driver_api::ConnectParams::inside_dbeaver_workspace

use std::ffi::OsStr;
use std::fmt;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock};

use ninesql_dbeaver::detect::SearchRoots;

/// Name of DBeaver's data directory in every known install location.
const DBEAVER_DATA: &str = "DBeaverData";

/// Most ancestors inspected for workspace / project markers.
const MAX_ANCESTORS: usize = 64;

/// Most entries of a `.dbeaver` directory listed while looking for a project file; a larger
/// directory counts as a project (fail closed).
const MAX_DBEAVER_ENTRIES: usize = 4096;

/// Result of [`DbeaverRoots::check`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct FileCheck {
    /// The canonical path that was checked (symbolic links resolved). `None` when the path
    /// could not be resolved (`inside` is then `true`).
    pub path: Option<PathBuf>,
    /// The file lies inside a DBeaver workspace, or the check could not tell.
    pub inside: bool,
}

/// The DBeaver workspace roots of this session. Cheap to clone; clones share the list, so a root
/// added after startup ([`DbeaverRoots::add`]) is seen by the session manager at once.
#[derive(Clone, Default)]
pub struct DbeaverRoots {
    roots: Arc<RwLock<Vec<PathBuf>>>,
    /// The user's home directory: the marker walk stops before it.
    home: Option<PathBuf>,
}

impl fmt::Debug for DbeaverRoots {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DbeaverRoots")
            .field("roots", &self.roots().len())
            .field("home", &self.home.is_some())
            .finish()
    }
}

impl DbeaverRoots {
    /// No known root and no home directory (the `DBeaverData` component rule and the marker
    /// detection still apply; the marker walk stops before `/`).
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// The `DBeaverData` directory of every known install location under `$HOME`; no known
    /// root when `HOME` is unset or relative. **Blocking** (canonicalizes `$HOME`).
    #[must_use]
    pub fn from_env() -> Self {
        SearchRoots::from_env().map_or_else(Self::none, |r| Self::from_search_roots(&r))
    }

    /// The `DBeaverData` directory of every known install location under `roots.home`, with
    /// `roots.home` as the home directory.
    #[must_use]
    pub fn from_search_roots(roots: &SearchRoots) -> Self {
        let out = Self::none().with_home(&roots.home);
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

    /// Sets the home directory: the marker walk never looks at it, its ancestors or `/`.
    /// A relative path is ignored. **Blocking** (canonicalizes the home directory): so are
    /// [`Self::from_env`] and [`Self::from_search_roots`], and with them `ManagerConfig::new`
    /// and `CoreConfig::new`, which must be built off the UI thread.
    #[must_use]
    pub fn with_home(mut self, home: impl AsRef<Path>) -> Self {
        let home = home.as_ref();
        if home.is_absolute() {
            self.home = Some(std::fs::canonicalize(home).unwrap_or_else(|_| normalize(home)));
        }
        self
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

    /// Whether the database file `path` lies inside a DBeaver workspace ([`Self::check`]).
    /// **Blocking**: run it on a blocking thread.
    #[must_use]
    pub fn contains(&self, path: &Path) -> bool {
        self.check(path).inside
    }

    /// Resolves `path` and tells whether it lies inside a DBeaver workspace (see the module
    /// documentation). **Blocking** (file system metadata): run it on a blocking thread.
    #[must_use]
    pub fn check(&self, path: &Path) -> FileCheck {
        let fail_closed = FileCheck {
            path: None,
            inside: true,
        };
        let Ok(absolute) = std::path::absolute(path) else {
            // No current directory to resolve a relative path against.
            return fail_closed;
        };
        let Ok(resolved) = resolve(&absolute) else {
            tracing::debug!("database file path cannot be resolved: treated as inside DBeaver");
            return fail_closed;
        };
        let lexical = normalize(&absolute);
        let inside = self.inside(&lexical, &resolved);
        FileCheck {
            path: Some(resolved),
            inside,
        }
    }

    fn inside(&self, lexical: &Path, resolved: &Path) -> bool {
        // The lexical path is checked too: an extra reason to be inside, never a way out.
        for candidate in [lexical, resolved] {
            if candidate
                .components()
                .any(|c| c.as_os_str() == OsStr::new(DBEAVER_DATA))
            {
                return true;
            }
        }
        for root in self.roots() {
            let root_lexical = normalize(&root);
            let root_resolved = std::fs::canonicalize(&root).ok();
            for candidate in [lexical, resolved] {
                if candidate.starts_with(&root_lexical)
                    || root_resolved
                        .as_deref()
                        .is_some_and(|r| candidate.starts_with(r))
                {
                    return true;
                }
            }
        }
        self.detected_marker(resolved)
    }

    /// Whether an ancestor of `path` (its parent first), below the home directory and `/`, is a
    /// DBeaver workspace or project folder. Fails closed.
    fn detected_marker(&self, path: &Path) -> bool {
        for dir in path.ancestors().skip(1).take(MAX_ANCESTORS) {
            if dir.parent().is_none() {
                break; // `/`
            }
            if let Some(home) = &self.home
                && home.starts_with(dir)
            {
                break; // `$HOME` or one of its ancestors: not a project, nor anything above.
            }
            if exists(&dir.join(".metadata/dbeaver-workspace.properties")) != Presence::Absent
                || dbeaver_project_dir(&dir.join(".dbeaver")) != Presence::Absent
            {
                return true;
            }
        }
        false
    }
}

/// Outcome of a marker check; [`Presence::Unknown`] counts as present (fail closed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Presence {
    Present,
    Absent,
    Unknown,
}

/// "Does not exist" for a path: not found, or a component that is not a directory.
fn is_missing(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    )
}

fn exists(path: &Path) -> Presence {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Presence::Present,
        Err(e) if is_missing(&e) => Presence::Absent,
        Err(_) => Presence::Unknown,
    }
}

/// Whether `dot_dbeaver` is a directory holding a DBeaver project file.
fn dbeaver_project_dir(dot_dbeaver: &Path) -> Presence {
    match std::fs::metadata(dot_dbeaver) {
        Ok(m) if m.is_dir() => {}
        Ok(_) => return Presence::Absent,
        Err(e) if is_missing(&e) => return Presence::Absent,
        Err(_) => return Presence::Unknown,
    }
    let Ok(entries) = std::fs::read_dir(dot_dbeaver) else {
        return Presence::Unknown;
    };
    for (i, entry) in entries.enumerate() {
        if i >= MAX_DBEAVER_ENTRIES {
            return Presence::Unknown;
        }
        let Ok(entry) = entry else {
            return Presence::Unknown;
        };
        if is_project_file(&entry.file_name()) {
            return Presence::Present;
        }
    }
    Presence::Absent
}

fn is_project_file(name: &OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    name == "project-settings.json"
        || name == "credentials-config.json"
        || (name.starts_with("data-sources") && name.ends_with(".json"))
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

/// Most symbolic links followed by hand while resolving a path whose target does not exist yet
/// (the same bound as Linux's `MAXSYMLINKS`).
const MAX_LINK_HOPS: usize = 40;

/// The path with symbolic links resolved: the canonical file, or, when it does not exist yet,
/// the canonical nearest existing ancestor followed by the missing names. A **dangling symbolic
/// link** on the way (the file, or a missing directory, is a link to something that does not
/// exist yet) is followed to its target, at most [`MAX_LINK_HOPS`] links deep: a creating open
/// would write there, so that is the path to check (review P1-D wiring, N1). `Err` for any other
/// failure, for a missing component that is not a plain name (`..`), and for too many links.
fn resolve(absolute: &Path) -> Result<PathBuf, ()> {
    resolve_hops(absolute, MAX_LINK_HOPS)
}

fn resolve_hops(absolute: &Path, hops: usize) -> Result<PathBuf, ()> {
    match std::fs::canonicalize(absolute) {
        Ok(p) => return Ok(p),
        Err(e) if is_missing(&e) => {}
        Err(_) => return Err(()),
    }
    let with_missing = |mut p: PathBuf, missing: &[std::ffi::OsString]| {
        for name in missing.iter().rev() {
            p.push(name);
        }
        p
    };
    let mut missing = Vec::new();
    let mut current = absolute.to_path_buf();
    loop {
        // Intermediate components are followed by the kernel; only the last one of `current`
        // can be a dangling link here.
        match std::fs::symlink_metadata(&current) {
            Ok(m) if m.file_type().is_symlink() => {
                let hops = hops.checked_sub(1).ok_or(())?;
                let target = std::fs::read_link(&current).map_err(drop)?;
                let target = if target.is_absolute() {
                    target
                } else {
                    current.parent().ok_or(())?.join(target)
                };
                return Ok(with_missing(resolve_hops(&target, hops)?, &missing));
            }
            Ok(_) if !missing.is_empty() => {
                // The nearest existing ancestor (not a link).
                let p = std::fs::canonicalize(&current).map_err(drop)?;
                return Ok(with_missing(p, &missing));
            }
            // The whole path exists now, though it was not found a moment ago: fail closed.
            Ok(_) => return Err(()),
            Err(e) if is_missing(&e) => {}
            Err(_) => return Err(()),
        }
        // `None` for a path ending in `..`: cannot be resolved without the file system.
        let name = current.file_name().ok_or(())?.to_owned();
        missing.push(name);
        current = current.parent().ok_or(())?.to_path_buf();
    }
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
        // A `.dbeaver` directory without a project file is not a project.
        assert!(!roots.contains(&project.join("a.sqlite")));
        for file in [
            "data-sources.json",
            "data-sources-2.json",
            "project-settings.json",
            "credentials-config.json",
        ] {
            let p = home.path().join(format!("project-{file}"));
            std::fs::create_dir_all(p.join(".dbeaver")).unwrap();
            std::fs::write(p.join(".dbeaver").join(file), b"{}").unwrap();
            assert!(roots.contains(&p.join("db/a.sqlite")), "{file}");
        }

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

    /// Review P1-D wiring, L3: `/tmp/.dbeaver` or `~/.dbeaver` do not make every file below
    /// read-only.
    #[test]
    fn stray_dbeaver_folders_in_home_or_tmp_do_not_capture_everything() {
        let base = tempfile::tempdir().unwrap();
        // Stands in for `/tmp`: a `.dbeaver` without project files.
        let tmp = base.path().join("tmp");
        std::fs::create_dir_all(tmp.join(".dbeaver")).unwrap();
        let roots = DbeaverRoots::none();
        assert!(!roots.contains(&tmp.join("a.db")));
        assert!(!roots.contains(&tmp.join("sub/a.db")));

        // `~/.dbeaver`, with or without project files, never makes home files inside.
        let home = base.path().join("home");
        std::fs::create_dir_all(home.join(".dbeaver")).unwrap();
        let roots = DbeaverRoots::none().with_home(&home);
        assert!(!roots.contains(&home.join("a.db")));
        assert!(!roots.contains(&home.join("data/a.db")));
        std::fs::write(home.join(".dbeaver/project-settings.json"), b"{}").unwrap();
        assert!(!roots.contains(&home.join("data/a.db")));
        // Nor does a workspace marker in the home directory itself.
        std::fs::create_dir_all(home.join(".metadata")).unwrap();
        std::fs::write(home.join(".metadata/dbeaver-workspace.properties"), b"").unwrap();
        assert!(!roots.contains(&home.join("data/a.db")));
        // A real project below the home directory still counts.
        let project = home.join("projects/General");
        std::fs::create_dir_all(project.join(".dbeaver")).unwrap();
        std::fs::write(project.join(".dbeaver/data-sources.json"), b"{}").unwrap();
        assert!(roots.contains(&project.join("db/a.db")));
    }

    /// Review P1-D wiring, L4 / L5: the checked path is canonical; a path that cannot be
    /// resolved counts as inside.
    #[test]
    fn the_checked_path_is_canonical_and_unresolvable_paths_fail_closed() {
        let base = tempfile::tempdir().unwrap();
        let real = base.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("a.db"), b"").unwrap();
        let link = base.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let canonical_real = std::fs::canonicalize(&real).unwrap();
        let roots = DbeaverRoots::none();

        let check = roots.check(&link.join("a.db"));
        assert_eq!(check.path, Some(canonical_real.join("a.db")));
        assert!(!check.inside);
        // A file that does not exist yet: its canonical parent plus its name.
        let check = roots.check(&link.join("new/b.db"));
        assert_eq!(check.path, Some(canonical_real.join("new/b.db")));
        assert!(!check.inside);

        // A symbolic link loop cannot be resolved: inside, no path.
        let lp = base.path().join("loop");
        std::os::unix::fs::symlink(&lp, &lp).unwrap();
        let check = roots.check(&lp.join("x.db"));
        assert_eq!(
            check,
            FileCheck {
                path: None,
                inside: true
            }
        );
        // `..` after a missing component cannot be resolved without the file system.
        let check = roots.check(&base.path().join("missing/.."));
        assert!(check.inside);
        assert!(check.path.is_none());
    }

    /// Review P1-D wiring, N1: a dangling symbolic link is checked at its target, where a
    /// creating open would write.
    #[test]
    fn a_dangling_link_is_checked_at_its_target() {
        let base = tempfile::tempdir().unwrap();
        let canonical = std::fs::canonicalize(base.path()).unwrap();
        let home = base.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let roots = DbeaverRoots::none().with_home(&home);

        // `~/x.db` → a file not created yet under `DBeaverData`.
        let target = base
            .path()
            .join("app/DBeaverData/workspace6/General/new.db");
        std::os::unix::fs::symlink(&target, home.join("x.db")).unwrap();
        let check = roots.check(&home.join("x.db"));
        assert!(check.inside);
        assert_eq!(
            check.path,
            Some(canonical.join("app/DBeaverData/workspace6/General/new.db"))
        );

        // A relative link, through a dangling link to a directory, into a root added later.
        let ws = base.path().join("custom-ws");
        roots.add(&ws);
        std::os::unix::fs::symlink("../custom-ws/General", home.join("dir")).unwrap();
        let check = roots.check(&home.join("dir/sub/y.db"));
        assert!(check.inside, "{check:?}");
        assert_eq!(
            check.path,
            Some(canonical.join("custom-ws/General/sub/y.db"))
        );

        // A dangling link outside every workspace stays outside, at its target.
        std::os::unix::fs::symlink(base.path().join("plain/z.db"), home.join("z.db")).unwrap();
        let check = roots.check(&home.join("z.db"));
        assert!(!check.inside);
        assert_eq!(check.path, Some(canonical.join("plain/z.db")));

        // A chain of dangling links longer than the bound fails closed.
        let mut next = base.path().join("chain-end/missing.db");
        for i in 0..=MAX_LINK_HOPS {
            let link = base.path().join(format!("chain-{i}"));
            std::os::unix::fs::symlink(&next, &link).unwrap();
            next = link;
        }
        assert_eq!(
            roots.check(&next),
            FileCheck {
                path: None,
                inside: true
            }
        );
    }
}
