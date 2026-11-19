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
//! **Document portal (P6-A review M1).** A file chosen through the Flatpak document portal has a
//! path under the portal's mount (`$XDG_RUNTIME_DIR/doc/<id>/<name>`, `/run/user/<uid>/doc/…`
//! or, in the sandbox, `/run/flatpak/doc/…`) that says nothing about where the file really is,
//! and the portal writes through to the host. Such a path is mapped to its host path with
//! `org.freedesktop.portal.Documents.GetHostPaths` (Documents portal version 5, session bus,
//! bounded to [`PORTAL_TIMEOUT`]; feature `document-portal`, on by default; answers cached per
//! document id, at most 256 ids, 10 min for a host path and 30 s for "no answer"), and the checks
//! above run on the host path. The local checks run on the portal path itself first: a
//! `DBeaverData` component, a known root or a marker visible through the portal's mount (a
//! workspace folder picked through the folder portal) is inside, never a way out. Then:
//! - the portal is unavailable, times out, fails, does not know the document, or returns a path
//!   that does not match the document's name: the file counts as inside (fail closed), with
//!   [`FileCheck::unverified`] set;
//! - a known root, a `DBeaverData` component or a marker found on the host path: inside;
//! - the marker walk cannot see an ancestor directory of the host path (it is not visible from
//!   this process), or runs inside a Flatpak sandbox (whose view of the file system is not the
//!   host's: a missing marker proves nothing): inside, with [`FileCheck::unverified`] set. Only
//!   the walk's stop at the home directory is unaffected, so a file directly in `$HOME` is
//!   outside;
//! - a root added with [`DbeaverRoots::add`] that is itself a document-portal path (a workspace
//!   picked through the portal for an import) is compared through its host path too.
//!
//! [`FileCheck::unverified`] lets a caller that only ever **creates a new file** (an export)
//! write where the check could not tell, as it did before the portal rule; opening a database
//! file, which can modify it, always reads [`FileCheck::inside`].
//!
//! [`ConnectParams::inside_dbeaver_workspace`]: ninesql_driver_api::ConnectParams::inside_dbeaver_workspace

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use ninesql_dbeaver::detect::SearchRoots;

/// Name of DBeaver's data directory in every known install location.
const DBEAVER_DATA: &str = "DBeaverData";

/// Most ancestors inspected for workspace / project markers.
const MAX_ANCESTORS: usize = 64;

/// Most entries of a `.dbeaver` directory listed while looking for a project file; a larger
/// directory counts as a project (fail closed).
const MAX_DBEAVER_ENTRIES: usize = 4096;

/// Longest wait for the Documents portal's answer (connection included); past it the file counts
/// as inside a DBeaver workspace.
pub const PORTAL_TIMEOUT: Duration = Duration::from_secs(2);

/// The document portal's mount inside a Flatpak sandbox.
const FLATPAK_DOC_ROOT: &str = "/run/flatpak/doc";

/// Result of [`DbeaverRoots::check`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct FileCheck {
    /// The canonical path that was checked (symbolic links resolved). `None` when the path
    /// could not be resolved (`inside` is then `true`).
    pub path: Option<PathBuf>,
    /// The file lies inside a DBeaver workspace, or the check could not tell.
    pub inside: bool,
    /// `inside` only because the check could not tell: a document-portal path whose host path
    /// could not be resolved or whose host directories cannot be inspected from here (see the
    /// module documentation). Always `false` when `inside` is `false`. A caller that only
    /// creates a new file may treat such a path as outside; a database open never does.
    pub unverified: bool,
}

impl FileCheck {
    fn fail_closed() -> Self {
        Self {
            path: None,
            inside: true,
            unverified: false,
        }
    }

    fn checked(path: PathBuf, inside: bool) -> Self {
        Self {
            path: Some(path),
            inside,
            unverified: false,
        }
    }

    fn unverified(path: PathBuf) -> Self {
        Self {
            path: Some(path),
            inside: true,
            unverified: true,
        }
    }
}

/// The DBeaver workspace roots of this session. Cheap to clone; clones share the list, so a root
/// added after startup ([`DbeaverRoots::add`]) is seen by the session manager at once.
#[derive(Clone, Default)]
pub struct DbeaverRoots {
    roots: Arc<RwLock<Vec<PathBuf>>>,
    /// The user's home directory: the marker walk stops before it.
    home: Option<PathBuf>,
    /// The document portal's environment; `None`: read from the process environment at each
    /// check. Set by tests.
    portal: Option<Arc<PortalEnv>>,
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
        let Ok(absolute) = std::path::absolute(path) else {
            // No current directory to resolve a relative path against.
            return FileCheck::fail_closed();
        };
        let Ok(resolved) = resolve(&absolute) else {
            tracing::debug!("database file path cannot be resolved: treated as inside DBeaver");
            return FileCheck::fail_closed();
        };
        let lexical = normalize(&absolute);
        let portal = self.portal_env();
        match portal.locate(&resolved) {
            DocLocation::Document(doc) => {
                return self.check_document(&portal, &lexical, resolved, &doc);
            }
            DocLocation::Malformed => {
                tracing::debug!("malformed document-portal path: treated as inside DBeaver");
                return FileCheck::checked(resolved, true);
            }
            DocLocation::NotDocument => {}
        }
        if !matches!(portal.locate(&lexical), DocLocation::NotDocument) {
            // Named through the portal's mount, but resolved elsewhere: fail closed.
            return FileCheck::checked(resolved, true);
        }
        // The lexical path is checked too: an extra reason to be inside, never a way out. A
        // document-portal root whose host path is unknown cannot hold a local path other than
        // through its own mount, which the lexical comparison covers.
        let inside = self.lexical_match(&[&lexical, &resolved], &portal) == Match::Inside
            || self.marker_walk(&resolved, Walk::Local) != Marker::Absent;
        FileCheck::checked(resolved, inside)
    }

    /// Whether `path` stops the creation of a **new** file there: inside a
    /// DBeaver workspace as far as the check can tell ([`FileCheck::inside`] without
    /// [`FileCheck::unverified`]). For callers that never replace or modify an existing file
    /// (the DBeaver export). **Blocking**.
    #[must_use]
    pub fn forbids_new_file(&self, path: &Path) -> bool {
        let check = self.check(path);
        check.inside && !check.unverified
    }

    /// The check of a document-portal path (`resolved`, under the portal's mount): the checks
    /// run on its host path. See the module documentation.
    fn check_document(
        &self,
        portal: &PortalEnv,
        lexical: &Path,
        resolved: PathBuf,
        doc: &DocumentPath,
    ) -> FileCheck {
        // The local checks on the portal path itself first (re-review N1): a root, a
        // `DBeaverData` component or a marker visible through the portal's mount (a workspace
        // folder picked through the folder portal) is a verified inside. Never a way out.
        if self.lexical_match(&[lexical, &resolved], portal) == Match::Inside
            || self.marker_walk(&resolved, Walk::Local) != Marker::Absent
        {
            return FileCheck::checked(resolved, true);
        }
        let Some(host) = portal.host_path(doc) else {
            tracing::debug!(
                "document-portal path without a host path: treated as inside DBeaver (unverified)"
            );
            return FileCheck::unverified(resolved);
        };
        if !matches!(portal.locate(&host), DocLocation::NotDocument) {
            // A document of a document: nothing to check against.
            return FileCheck::checked(resolved, true);
        }
        // Outside a sandbox the host path is this process's own view: resolve its links too.
        // Inside one, the sandbox's view of that name may be another file (`/tmp`): never.
        let host_resolved = if portal.sandboxed {
            None
        } else {
            resolve(&host).ok()
        };
        let mut candidates = vec![host.as_path()];
        candidates.extend(host_resolved.as_deref());
        match self.lexical_match(&candidates, portal) {
            Match::Inside => return FileCheck::checked(resolved, true),
            Match::Unresolved => return FileCheck::unverified(resolved),
            Match::Outside => {}
        }
        let walked = host_resolved.as_deref().unwrap_or(&host);
        match self.marker_walk(
            walked,
            Walk::Host {
                sandboxed: portal.sandboxed,
            },
        ) {
            Marker::Found => FileCheck::checked(resolved, true),
            Marker::Unverified => FileCheck::unverified(resolved),
            Marker::Absent => FileCheck::checked(resolved, false),
        }
    }

    fn portal_env(&self) -> Arc<PortalEnv> {
        self.portal
            .clone()
            .unwrap_or_else(|| Arc::new(PortalEnv::from_env()))
    }

    /// Whether a candidate path has a `DBeaverData` component or lies under a known root.
    /// [`Match::Unresolved`]: no match, but a root is a document-portal path whose host path is
    /// unknown.
    fn lexical_match(&self, candidates: &[&Path], portal: &PortalEnv) -> Match {
        for candidate in candidates {
            if candidate
                .components()
                .any(|c| c.as_os_str() == OsStr::new(DBEAVER_DATA))
            {
                return Match::Inside;
            }
        }
        let mut unresolved = false;
        for root in self.roots() {
            let root_lexical = normalize(&root);
            let root_resolved = std::fs::canonicalize(&root).ok();
            let mut root_host = None;
            let located = portal.locate(root_resolved.as_deref().unwrap_or(&root_lexical));
            match located {
                DocLocation::Document(doc) => match portal.host_path(&doc) {
                    Some(host) => root_host = Some(host),
                    None => unresolved = true,
                },
                DocLocation::Malformed => unresolved = true,
                DocLocation::NotDocument => {}
            }
            for candidate in candidates {
                if candidate.starts_with(&root_lexical)
                    || root_resolved
                        .as_deref()
                        .is_some_and(|r| candidate.starts_with(r))
                    || root_host
                        .as_deref()
                        .is_some_and(|r| candidate.starts_with(r))
                {
                    return Match::Inside;
                }
            }
        }
        if unresolved {
            Match::Unresolved
        } else {
            Match::Outside
        }
    }

    /// Whether an ancestor of `path` (its parent first), below the home directory and `/`, is a
    /// DBeaver workspace or project folder. Fails closed: a marker whose presence cannot be
    /// checked is [`Marker::Found`]. On a host path ([`Walk::Host`]), an ancestor that is not
    /// visible from here, or any ancestor inside a sandbox, makes a walk that finds nothing
    /// [`Marker::Unverified`].
    fn marker_walk(&self, path: &Path, walk: Walk) -> Marker {
        let mut unverified = false;
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
                return Marker::Found;
            }
            if let Walk::Host { sandboxed } = walk
                && (sandboxed || !std::fs::metadata(dir).is_ok_and(|m| m.is_dir()))
            {
                unverified = true;
            }
        }
        if unverified {
            Marker::Unverified
        } else {
            Marker::Absent
        }
    }
}

/// Outcome of [`DbeaverRoots::lexical_match`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Match {
    Inside,
    Unresolved,
    Outside,
}

/// Outcome of [`DbeaverRoots::marker_walk`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Marker {
    Found,
    Unverified,
    Absent,
}

/// What [`DbeaverRoots::marker_walk`] walks: a path of this process's own file system, or the
/// host path of a document-portal file.
#[derive(Debug, Clone, Copy)]
enum Walk {
    Local,
    Host { sandboxed: bool },
}

/// Answers "which host path is document `id`?"; `None` when it cannot tell.
type HostPathResolver = Arc<dyn Fn(&str) -> Option<PathBuf> + Send + Sync>;

/// The document portal as seen from this process.
struct PortalEnv {
    /// Mounts of the document portal besides `/run/flatpak/doc` and `/run/user/<uid>/doc`
    /// (always recognised): `$XDG_RUNTIME_DIR/doc`, as given and canonical.
    doc_roots: Vec<PathBuf>,
    /// Running inside a Flatpak sandbox (`FLATPAK_ID` set, or `/.flatpak-info` present or
    /// unreadable).
    sandboxed: bool,
    resolver: HostPathResolver,
}

impl PortalEnv {
    /// **Blocking** (`stat` of `/.flatpak-info`, `canonicalize` of `$XDG_RUNTIME_DIR`).
    fn from_env() -> Self {
        let mut doc_roots = Vec::new();
        if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from)
            && runtime.is_absolute()
        {
            let doc = runtime.join("doc");
            if let Ok(canonical) = std::fs::canonicalize(&doc)
                && canonical != doc
            {
                doc_roots.push(canonical);
            }
            doc_roots.push(normalize(&doc));
        }
        let flatpak_id = std::env::var_os("FLATPAK_ID").is_some_and(|v| !v.is_empty());
        let info = match std::fs::symlink_metadata("/.flatpak-info") {
            Ok(_) => true,
            Err(e) => !is_missing(&e), // Unknown: the stricter answer.
        };
        Self {
            doc_roots,
            sandboxed: flatpak_id || info,
            resolver: Arc::new(portal_host_path),
        }
    }

    /// Where `path` (absolute, normalized) lies with respect to the portal's mounts.
    fn locate(&self, path: &Path) -> DocLocation {
        let Some(rel) = self.doc_relative(path) else {
            return DocLocation::NotDocument;
        };
        parse_document(rel).map_or(DocLocation::Malformed, DocLocation::Document)
    }

    fn doc_relative<'a>(&self, path: &'a Path) -> Option<&'a Path> {
        for root in self
            .doc_roots
            .iter()
            .map(PathBuf::as_path)
            .chain([Path::new(FLATPAK_DOC_ROOT)])
        {
            if let Ok(rel) = path.strip_prefix(root) {
                return Some(rel);
            }
        }
        // `/run/user/<uid>/doc`, whatever `XDG_RUNTIME_DIR` says.
        let mut c = path.components();
        let ok = c.next() == Some(Component::RootDir)
            && c.next() == Some(Component::Normal(OsStr::new("run")))
            && c.next() == Some(Component::Normal(OsStr::new("user")))
            && matches!(c.next(), Some(Component::Normal(uid))
                if uid.to_str().is_some_and(|u| !u.is_empty() && u.bytes().all(|b| b.is_ascii_digit())))
            && c.next() == Some(Component::Normal(OsStr::new("doc")));
        ok.then_some(c.as_path())
    }

    /// The host path of `doc`, or `None` (portal unavailable, unknown document, an answer that
    /// is not absolute, holds `.` / `..`, or does not end with the document's name).
    fn host_path(&self, doc: &DocumentPath) -> Option<PathBuf> {
        let host = (self.resolver)(&doc.id)?;
        if !host.is_absolute()
            || host
                .components()
                .any(|c| matches!(c, Component::CurDir | Component::ParentDir))
            || host.file_name() != Some(doc.name.as_os_str())
        {
            tracing::debug!("document portal answer does not match the document");
            return None;
        }
        Some(if doc.rest.as_os_str().is_empty() {
            host
        } else {
            host.join(&doc.rest)
        })
    }
}

/// Where a path lies with respect to the document portal's mounts.
#[derive(Debug)]
enum DocLocation {
    NotDocument,
    /// Under a mount, but not `<id>/<name>[/…]` (or `by-app/<app>/<id>/<name>[/…]`).
    Malformed,
    Document(DocumentPath),
}

/// A path inside the document portal: `<id>/<name>/<rest>`.
#[derive(Debug, PartialEq, Eq)]
struct DocumentPath {
    /// The document id.
    id: String,
    /// The exported file or directory's own name (the last component of its host path).
    name: OsString,
    /// The rest of the path, below an exported directory (empty for a file).
    rest: PathBuf,
}

fn parse_document(rel: &Path) -> Option<DocumentPath> {
    let mut parts = rel.components();
    let mut next = || match parts.next() {
        Some(Component::Normal(n)) => Some(n.to_owned()),
        _ => None,
    };
    let mut id = next()?;
    if id == "by-app" {
        let _app = next()?;
        id = next()?;
    }
    let name = next()?;
    let id = id.into_string().ok()?;
    let valid_id = !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if !valid_id {
        return None;
    }
    let rest = parts.as_path();
    if rest
        .components()
        .any(|c| !matches!(c, Component::Normal(_)))
    {
        return None;
    }
    Some(DocumentPath {
        id,
        name,
        rest: rest.to_path_buf(),
    })
}

/// How long the portal's answer for a document id is reused. Ids are stable for the life of a
/// document; the bound only limits how long a removed and re-created document could be served
/// from the cache.
const CACHE_FOUND_TTL: Duration = Duration::from_secs(600);
/// How long "no answer" is reused, so that checks against a portal root whose host path is
/// unknown do not call the bus again and again (re-review N2). Still fail closed meanwhile.
const CACHE_MISSING_TTL: Duration = Duration::from_secs(30);
/// Most document ids kept; the oldest entry goes first.
const CACHE_MAX_ENTRIES: usize = 256;

/// Answers of the Documents portal per document id, bounded in size and age.
#[derive(Default)]
#[cfg_attr(not(feature = "document-portal"), allow(dead_code))]
struct HostPathCache {
    entries: std::collections::HashMap<String, (Option<PathBuf>, std::time::Instant)>,
}

#[cfg_attr(not(feature = "document-portal"), allow(dead_code))]
impl HostPathCache {
    /// The cached answer for `id` at `now` (`Some(None)`: "no answer", still fresh), or `None`
    /// when nothing fresh is cached.
    fn lookup(&self, id: &str, now: std::time::Instant) -> Option<Option<PathBuf>> {
        let (answer, at) = self.entries.get(id)?;
        let ttl = if answer.is_some() {
            CACHE_FOUND_TTL
        } else {
            CACHE_MISSING_TTL
        };
        (now.saturating_duration_since(*at) < ttl).then(|| answer.clone())
    }

    /// Keeps `answer` for `id`, dropping the oldest entry when full.
    fn insert(&mut self, id: &str, answer: Option<PathBuf>, now: std::time::Instant) {
        if !self.entries.contains_key(id) && self.entries.len() >= CACHE_MAX_ENTRIES {
            let oldest = self
                .entries
                .iter()
                .min_by_key(|(_, (_, at))| *at)
                .map(|(k, _)| k.clone());
            if let Some(oldest) = oldest {
                self.entries.remove(&oldest);
            }
        }
        self.entries.insert(id.to_owned(), (answer, now));
    }
}

/// Host path of document `id`, from [`HostPathCache`] or the portal. The cache lock is not held
/// during the bus call: two checks of a new id may both ask the portal.
#[cfg(feature = "document-portal")]
fn portal_host_path(id: &str) -> Option<PathBuf> {
    static CACHE: std::sync::LazyLock<std::sync::Mutex<HostPathCache>> =
        std::sync::LazyLock::new(Default::default);
    let cache = || CACHE.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(answer) = cache().lookup(id, std::time::Instant::now()) {
        return answer;
    }
    let answer = portal_host_path_uncached(id);
    cache().insert(id, answer.clone(), std::time::Instant::now());
    answer
}

/// Host path of document `id` through the Documents portal (version 5 `GetHostPaths`), on a
/// thread of its own, waited for at most [`PORTAL_TIMEOUT`]. At most [`MAX_PORTAL_PENDING`]
/// calls in flight: past that (a portal that hangs), `None` at once.
#[cfg(feature = "document-portal")]
fn portal_host_path_uncached(id: &str) -> Option<PathBuf> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static PENDING: AtomicUsize = AtomicUsize::new(0);
    if PENDING.fetch_add(1, Ordering::AcqRel) >= MAX_PORTAL_PENDING {
        PENDING.fetch_sub(1, Ordering::AcqRel);
        tracing::debug!("document portal calls pending: host path not resolved");
        return None;
    }
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let owned = id.to_owned();
    let spawned = std::thread::Builder::new()
        .name("ninesql-doc-portal".into())
        .spawn(move || {
            let answer = zbus::block_on(get_host_path(&owned));
            PENDING.fetch_sub(1, Ordering::AcqRel);
            let _ = tx.send(answer);
        });
    if spawned.is_err() {
        PENDING.fetch_sub(1, Ordering::AcqRel);
        return None;
    }
    match rx.recv_timeout(PORTAL_TIMEOUT) {
        Ok(answer) => answer,
        Err(_) => {
            tracing::debug!("document portal did not answer in time");
            None
        }
    }
}

/// Most Documents portal calls in flight at once.
#[cfg(feature = "document-portal")]
const MAX_PORTAL_PENDING: usize = 4;

#[cfg(feature = "document-portal")]
async fn get_host_path(id: &str) -> Option<PathBuf> {
    use std::collections::HashMap;
    let conn = zbus::connection::Builder::session()
        .method_timeout(PORTAL_TIMEOUT)
        .build()
        .await
        .map_err(|e| tracing::debug!(error = %e, "no session bus for the document portal"))
        .ok()?;
    let reply = conn
        .call_method(
            Some("org.freedesktop.portal.Documents"),
            "/org/freedesktop/portal/documents",
            Some("org.freedesktop.portal.Documents"),
            "GetHostPaths",
            &(vec![id],),
        )
        .await
        .map_err(|e| tracing::debug!(error = %e, "document portal GetHostPaths failed"))
        .ok()?;
    let paths: HashMap<String, Vec<u8>> = reply.body().deserialize().ok()?;
    host_path_from_bytes(paths.get(id)?)
}

/// Without the `document-portal` feature no host path is ever known: every document-portal path
/// counts as inside (fail closed).
#[cfg(not(feature = "document-portal"))]
fn portal_host_path(_id: &str) -> Option<PathBuf> {
    None
}

/// A portal byte string (`ay`, NUL-terminated) as a path.
#[cfg_attr(not(feature = "document-portal"), allow(dead_code))]
fn host_path_from_bytes(bytes: &[u8]) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let bytes = bytes.split(|&b| b == 0).next().unwrap_or_default();
    if bytes.is_empty() {
        return None;
    }
    Some(PathBuf::from(OsStr::from_bytes(bytes)))
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
        assert_eq!(check, FileCheck::fail_closed());
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
        assert_eq!(roots.check(&next), FileCheck::fail_closed());
    }

    // --- Document portal (P6-A review M1) ---------------------------------------------------

    /// `roots` with a synthetic document portal: `doc_roots` as extra mounts, `map` as the
    /// portal's answers (an id not listed: no answer).
    fn with_portal(
        roots: DbeaverRoots,
        doc_roots: Vec<PathBuf>,
        sandboxed: bool,
        map: Vec<(&str, PathBuf)>,
    ) -> DbeaverRoots {
        let map: Vec<(String, PathBuf)> = map.into_iter().map(|(k, v)| (k.to_owned(), v)).collect();
        DbeaverRoots {
            portal: Some(Arc::new(PortalEnv {
                doc_roots,
                sandboxed,
                resolver: Arc::new(move |id| {
                    map.iter().find(|(k, _)| k == id).map(|(_, v)| v.clone())
                }),
            })),
            ..roots
        }
    }

    fn env(sandboxed: bool) -> PortalEnv {
        PortalEnv {
            doc_roots: Vec::new(),
            sandboxed,
            resolver: Arc::new(|_| None),
        }
    }

    #[test]
    fn document_portal_paths_are_recognised() {
        let e = env(false);
        let doc = |p: &str| match e.locate(Path::new(p)) {
            DocLocation::Document(d) => Some(d),
            DocLocation::Malformed => None,
            DocLocation::NotDocument => panic!("{p} is a document path"),
        };
        assert_eq!(
            doc("/run/user/1000/doc/a1B2_c-3/x.db"),
            Some(DocumentPath {
                id: "a1B2_c-3".into(),
                name: "x.db".into(),
                rest: PathBuf::new(),
            })
        );
        assert_eq!(
            doc("/run/flatpak/doc/abc/ws/General/x.db"),
            Some(DocumentPath {
                id: "abc".into(),
                name: "ws".into(),
                rest: PathBuf::from("General/x.db"),
            })
        );
        assert_eq!(
            doc("/run/user/1000/doc/by-app/io.github.yil00.NineSQL/abc/x.db").map(|d| d.id),
            Some("abc".to_owned())
        );
        // Under the mount but not `<id>/<name>`: malformed (fail closed).
        for p in [
            "/run/user/1000/doc",
            "/run/user/1000/doc/abc",
            "/run/user/1000/doc/a.b/x.db",
            "/run/user/1000/doc/by-app/app",
            "/run/flatpak/doc/abc",
        ] {
            assert!(doc(p).is_none(), "{p}");
        }
        for p in [
            "/run/user/1000/docs/abc/x.db",
            "/run/user/me/doc/abc/x.db",
            "/run/user/doc/abc/x.db",
            "/home/u/run/user/1000/doc/abc/x.db",
            "/tmp/x.db",
        ] {
            assert!(
                matches!(e.locate(Path::new(p)), DocLocation::NotDocument),
                "{p}"
            );
        }
        assert_eq!(
            host_path_from_bytes(b"/home/u/a.db\0"),
            Some(PathBuf::from("/home/u/a.db"))
        );
        assert_eq!(host_path_from_bytes(b"\0"), None);
    }

    /// Without an answer from the portal (unavailable, timed out, failed, unknown document) or
    /// with an answer that does not match the document, the file counts as inside.
    #[test]
    fn a_document_portal_file_without_a_host_path_fails_closed() {
        let home = tempfile::tempdir().unwrap();
        let roots = with_portal(
            DbeaverRoots::none().with_home(home.path()),
            Vec::new(),
            false,
            vec![
                ("other", home.path().join("data/other.db")),
                ("relative", PathBuf::from("data/x.db")),
                ("dotted", home.path().join("data/../x.db")),
            ],
        );
        for p in [
            "/run/user/1000/doc/abc123/x.db",
            "/run/flatpak/doc/abc123/x.db",
            // The portal knows the id, but under another name.
            "/run/user/1000/doc/other/x.db",
            "/run/user/1000/doc/relative/x.db",
            "/run/user/1000/doc/dotted/x.db",
        ] {
            let check = roots.check(Path::new(p));
            assert!(check.inside && check.unverified, "{p}: {check:?}");
            // The driver still opens the portal path: the check and the open name one file.
            assert_eq!(check.path.as_deref(), Some(Path::new(p)), "{p}");
            assert!(roots.contains(Path::new(p)));
            assert!(!roots.forbids_new_file(Path::new(p)));
        }
        // A malformed portal path: inside, and not merely unverified.
        let check = roots.check(Path::new("/run/user/1000/doc/a.b/x.db"));
        assert!(check.inside && !check.unverified);

        // The real environment: whatever the portal answers (in CI, no session bus), a document
        // the portal does not know is inside.
        let real = DbeaverRoots::none();
        assert!(real.contains(Path::new("/run/user/1000/doc/zz-no-such-doc/x.db")));
    }

    #[test]
    fn a_document_portal_file_is_checked_at_its_host_path() {
        let home = tempfile::tempdir().unwrap();
        let h = std::fs::canonicalize(home.path()).unwrap();
        std::fs::create_dir_all(h.join("data")).unwrap();
        let project = h.join("projects/General");
        std::fs::create_dir_all(project.join(".dbeaver")).unwrap();
        std::fs::write(project.join(".dbeaver/data-sources.json"), b"{}").unwrap();
        let custom = h.join("custom-ws");
        std::fs::create_dir_all(custom.join("General")).unwrap();
        let base = DbeaverRoots::from_search_roots(&SearchRoots { home: h.clone() });
        base.add(&custom);
        let roots = with_portal(
            base,
            Vec::new(),
            false,
            vec![
                (
                    "native",
                    h.join(".local/share/DBeaverData/workspace6/General/x.db"),
                ),
                ("unknown-install", h.join("opt/DBeaverData/a.db")),
                ("project", project.join("db/a.db")),
                ("custom", custom.clone()),
                ("plain", h.join("data/app.db")),
                ("top", h.join("app.db")),
                ("hidden", h.join("not-visible/a.db")),
            ],
        );
        let doc = |p: &str| PathBuf::from(format!("/run/user/1000/doc/{p}"));

        // Known root, `DBeaverData` component, project marker, directory grant into a root.
        for p in [
            "native/x.db",
            "unknown-install/a.db",
            "project/a.db",
            "custom/custom-ws/General/a.db",
        ] {
            let check = roots.check(&doc(p));
            assert!(check.inside && !check.unverified, "{p}: {check:?}");
            assert!(roots.forbids_new_file(&doc(p)), "{p}");
        }
        // Visible host directories without a marker, or the home directory itself: outside.
        for p in ["plain/app.db", "top/app.db"] {
            let check = roots.check(&doc(p));
            assert!(!check.inside && !check.unverified, "{p}: {check:?}");
            assert_eq!(check.path, Some(doc(p)));
        }
        // A host directory this process cannot see: its markers cannot be checked.
        let check = roots.check(&doc("hidden/a.db"));
        assert!(check.inside && check.unverified, "{check:?}");
    }

    /// Inside a Flatpak sandbox a missing marker proves nothing: only a file directly in the
    /// home directory is outside, and a marker the sandbox does see still counts.
    #[test]
    fn inside_a_sandbox_a_document_portal_file_is_inside_unless_directly_in_home() {
        let home = tempfile::tempdir().unwrap();
        let h = std::fs::canonicalize(home.path()).unwrap();
        std::fs::create_dir_all(h.join("data")).unwrap();
        let project = h.join("granted/General");
        std::fs::create_dir_all(project.join(".dbeaver")).unwrap();
        std::fs::write(project.join(".dbeaver/project-settings.json"), b"{}").unwrap();
        let roots = with_portal(
            DbeaverRoots::none().with_home(&h),
            Vec::new(),
            true,
            vec![
                ("plain", h.join("data/app.db")),
                ("top", h.join("app.db")),
                ("project", project.join("a.db")),
                // Another directory than the test's home (under `/tmp`), so the walk does not stop.
                ("srv", PathBuf::from("/srv/ninesql-test/a.db")),
            ],
        );
        let doc = |p: &str| PathBuf::from(format!("/run/flatpak/doc/{p}"));
        for p in ["plain/app.db", "srv/a.db"] {
            let check = roots.check(&doc(p));
            assert!(check.inside && check.unverified, "{p}: {check:?}");
        }
        let check = roots.check(&doc("top/app.db"));
        assert!(!check.inside, "{check:?}");
        let check = roots.check(&doc("project/a.db"));
        assert!(check.inside && !check.unverified, "{check:?}");
    }

    /// A workspace picked through the portal for an import is compared through its host path;
    /// one whose host path is unknown makes portal files unverified.
    #[test]
    fn a_document_portal_root_is_compared_at_its_host_path() {
        let home = tempfile::tempdir().unwrap();
        let h = std::fs::canonicalize(home.path()).unwrap();
        let ws = h.join("dbeaver-ws");
        std::fs::create_dir_all(ws.join("General")).unwrap();
        std::fs::write(ws.join("General/a.db"), b"").unwrap();
        std::fs::create_dir_all(h.join("data")).unwrap();
        let base = DbeaverRoots::none().with_home(&h);
        base.add("/run/user/1000/doc/wsid/dbeaver-ws");
        let roots = with_portal(
            base,
            Vec::new(),
            false,
            vec![
                ("wsid", ws.clone()),
                ("file", ws.join("General/a.db")),
                ("plain", h.join("data/app.db")),
            ],
        );
        // A local path into the workspace, and another document inside it.
        assert!(roots.contains(&ws.join("General/a.db")));
        let check = roots.check(Path::new("/run/user/1000/doc/file/a.db"));
        assert!(check.inside && !check.unverified, "{check:?}");
        assert!(!roots.contains(Path::new("/run/user/1000/doc/plain/app.db")));

        // The same root, no longer known to the portal.
        let base = DbeaverRoots::none().with_home(&h);
        base.add("/run/user/1000/doc/wsid/dbeaver-ws");
        let roots = with_portal(
            base,
            Vec::new(),
            false,
            vec![("plain", h.join("data/app.db"))],
        );
        let check = roots.check(Path::new("/run/user/1000/doc/plain/app.db"));
        assert!(check.inside && check.unverified, "{check:?}");
        // A local file is unaffected (it is not under the root's mount).
        assert!(!roots.contains(&h.join("data/app.db")));
    }

    /// `$XDG_RUNTIME_DIR/doc` wherever it is, through a real file under the mount (symbolic
    /// links resolved first).
    #[test]
    fn a_custom_document_mount_is_recognised_after_resolving_links() {
        let base = tempfile::tempdir().unwrap();
        let b = std::fs::canonicalize(base.path()).unwrap();
        let mount = b.join("runtime/doc");
        std::fs::create_dir_all(mount.join("abc")).unwrap();
        std::fs::write(mount.join("abc/x.db"), b"").unwrap();
        let link = b.join("x-link.db");
        std::os::unix::fs::symlink(mount.join("abc/x.db"), &link).unwrap();
        let roots = with_portal(DbeaverRoots::none(), vec![mount.clone()], false, Vec::new());
        let check = roots.check(&link);
        assert!(check.inside && check.unverified, "{check:?}");
        assert_eq!(check.path, Some(mount.join("abc/x.db")));
        let roots = with_portal(
            DbeaverRoots::none(),
            vec![mount.clone()],
            false,
            vec![("abc", b.join("plain/x.db"))],
        );
        std::fs::create_dir_all(b.join("plain")).unwrap();
        assert!(!roots.contains(&link));
    }

    /// Re-review N1: a workspace picked through the folder portal, whose marker is visible
    /// through the portal's mount, is a verified inside, whatever the portal answers.
    #[test]
    fn markers_visible_through_the_portal_mount_are_a_verified_inside() {
        let base = tempfile::tempdir().unwrap();
        let b = std::fs::canonicalize(base.path()).unwrap();
        let mount = b.join("runtime/doc");
        let ws = mount.join("wsid/workspace6");
        std::fs::create_dir_all(ws.join(".metadata")).unwrap();
        std::fs::write(ws.join(".metadata/dbeaver-workspace.properties"), b"").unwrap();
        std::fs::create_dir_all(ws.join("General")).unwrap();
        let db = ws.join("General/a.db");
        std::fs::write(&db, b"").unwrap();
        std::fs::create_dir_all(b.join("plain")).unwrap();
        // No answer from the portal, then an answer that alone would say "outside".
        for map in [Vec::new(), vec![("wsid", b.join("plain/workspace6"))]] {
            let roots = with_portal(DbeaverRoots::none(), vec![mount.clone()], false, map);
            let check = roots.check(&db);
            assert!(check.inside && !check.unverified, "{check:?}");
            assert!(roots.forbids_new_file(&db));
        }
        // A `DBeaverData` component in the portal path itself.
        let named = mount.join("x/DBeaverData");
        std::fs::create_dir_all(&named).unwrap();
        let roots = with_portal(
            DbeaverRoots::none(),
            vec![mount.clone()],
            false,
            vec![("x", b.join("plain/DBeaverData"))],
        );
        let check = roots.check(&named.join("a.db"));
        assert!(check.inside && !check.unverified, "{check:?}");
    }

    /// Re-review N2: answers are reused per id, "no answer" for a shorter time, within a bound.
    #[test]
    fn portal_answers_are_cached_per_id_within_bounds() {
        let mut cache = HostPathCache::default();
        let t0 = std::time::Instant::now();
        assert_eq!(cache.lookup("a", t0), None);
        cache.insert("a", Some(PathBuf::from("/h/a.db")), t0);
        cache.insert("b", None, t0);
        assert_eq!(cache.lookup("a", t0), Some(Some(PathBuf::from("/h/a.db"))));
        assert_eq!(cache.lookup("b", t0), Some(None));
        let later = t0 + CACHE_MISSING_TTL;
        assert_eq!(cache.lookup("b", later), None, "a miss is asked again");
        assert!(cache.lookup("a", later).is_some());
        assert_eq!(cache.lookup("a", t0 + CACHE_FOUND_TTL), None);
        for i in 0..CACHE_MAX_ENTRIES + 10 {
            cache.insert(
                &format!("id{i}"),
                None,
                t0 + Duration::from_millis(u64::try_from(i).unwrap() + 1),
            );
        }
        assert_eq!(cache.entries.len(), CACHE_MAX_ENTRIES);
        // The oldest entries went first.
        assert!(!cache.entries.contains_key("a") && !cache.entries.contains_key("id0"));
        assert!(
            cache
                .entries
                .contains_key(&format!("id{}", CACHE_MAX_ENTRIES + 9))
        );
    }
}
