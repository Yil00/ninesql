//! Script files on disk (F-SQL-11): the project's `scripts/` folder and its `scripts.json`
//! links. **Blocking**: every function here touches the file system and runs on a background
//! thread, never on the UI thread (I6).
//!
//! Rules:
//! - **Inside `scripts/` only.** A [`ScriptPath`] is relative, without `..`, hidden names or
//!   control characters (`ninesql_editor::script`). On disk, every folder between `scripts/`
//!   and the file must be a real directory: a symbolic link anywhere on the way (or in place of
//!   the file) is refused, never followed, and the resolved parent must still lie under the
//!   resolved `scripts/` (defence in depth). `scripts/` itself must be a real directory owned
//!   by the user (`ensure_private_dir`).
//! - **Never inside a DBeaver workspace (I4).** `scripts/` and the file are checked with the
//!   session manager's [`DbeaverRoots`] (fail closed: a path that cannot be resolved counts as
//!   inside), so a project that lives in, or is linked into, a DBeaver workspace is neither
//!   read nor written.
//! - **Safe writes.** `write_atomic`: a `0600` temp file next to the script, `fsync`, rename;
//!   new folders are `0700`. A script is never written through a symbolic link.
//! - **Bounded reads.** [`MAX_SCRIPT_BYTES`] per file; FIFOs and devices are refused without
//!   blocking; the text must be UTF-8. The editor then strips control characters and
//!   normalises line ends on load (ADR-0025 §3).
//! - **Bounded listing.** At most [`MAX_LISTED`] scripts, [`MAX_LIST_ENTRIES`] directory
//!   entries looked at, folders up to the script path depth; symbolic links and hidden entries
//!   (the atomic writer's temp files among them) are skipped.
//!
//! Residual: the checks and the open are separate system calls. Swapping a folder of
//! `scripts/` for a symbolic link between them needs write access to the user's own `0700`
//! NineSQL folders; the final open never follows a link (`O_NOFOLLOW`).

use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::UNIX_EPOCH;

use ninesql_core::DbeaverRoots;
use ninesql_core::storage::fs::{
    ensure_private_dir, read_limited, read_private, write_atomic, write_new_atomic,
};
use ninesql_core::storage::{ProjectPaths, StorageError};
use ninesql_editor::script::{FileStamp, MAX_SCRIPT_PATH_DEPTH, ScriptLinks, ScriptPath};

/// Largest script file opened: 8 MiB. The editor keeps the whole text in memory and some of its
/// work on an edit runs on the UI thread over the whole text (the hygiene and bidirectional
/// scans, the unsaved check, the tree-sitter reparse); 8 MiB keeps that within a frame or two
/// and is far above any hand-written script. Larger dumps are imported or run from a file, not
/// edited.
pub const MAX_SCRIPT_BYTES: u64 = 8 * 1024 * 1024;

/// Most scripts listed by "Open script…".
pub const MAX_LISTED: usize = 5_000;

/// Most directory entries looked at while listing `scripts/`.
pub const MAX_LIST_ENTRIES: usize = 50_000;

/// Largest `scripts.json` read.
const MAX_LINKS_BYTES: u64 = 4 * 1024 * 1024;

/// Serializes the read-modify-write of `scripts.json` within the process.
static LINKS_LOCK: Mutex<()> = Mutex::new(());

/// Why a script could not be read or written. Display texts are user-facing.
#[derive(Debug, thiserror::Error)]
pub enum ScriptFileError {
    /// No project is open.
    #[error("No project is open: scripts cannot be saved or opened.")]
    NoProject,
    /// The file does not exist.
    #[error("\"{0}\" does not exist in the scripts folder.")]
    NotFound(String),
    /// A symbolic link on the way or in place of the file.
    #[error(
        "\"{0}\" goes through a symbolic link: only real folders and files inside the scripts folder are used."
    )]
    Symlink(String),
    /// Not a regular file, or a folder expected.
    #[error("\"{0}\" is not a regular file or folder.")]
    WrongType(String),
    /// Resolves outside `scripts/`.
    #[error("\"{0}\" is outside the scripts folder.")]
    Outside(String),
    /// Inside a DBeaver workspace (I4).
    #[error(
        "\"{0}\" is inside a DBeaver workspace: NineSQL never reads scripts from or writes them to DBeaver's files."
    )]
    InsideDbeaver(String),
    /// Over [`MAX_SCRIPT_BYTES`].
    #[error("\"{name}\" is {size} bytes, over the {limit}-byte limit of the SQL editor.")]
    TooLarge {
        /// Script.
        name: String,
        /// Size.
        size: u64,
        /// Limit.
        limit: u64,
    },
    /// Not UTF-8.
    #[error("\"{0}\" is not UTF-8 text.")]
    NotUtf8(String),
    /// Save as: the name is taken.
    #[error("\"{0}\" already exists: choose another name.")]
    Exists(String),
    /// "Save": the file changed (or disappeared) since it was loaded or last saved here.
    #[error(
        "\"{0}\" changed on disk since it was opened or saved here (another window or another program)."
    )]
    Changed(String),
    /// `scripts.json` written by a newer NineSQL, or invalid.
    #[error("scripts.json cannot be used ({0}): the script's connection is not remembered.")]
    Links(String),
    /// Any other file system error.
    #[error("\"{name}\": {detail}")]
    Io {
        /// Script.
        name: String,
        /// Cause.
        detail: String,
    },
}

fn shown(path: &ScriptPath) -> String {
    crate::browse::display_name(&path.as_path().to_string_lossy())
}

fn io_err(path: &ScriptPath, e: &io::Error) -> ScriptFileError {
    ScriptFileError::Io {
        name: shown(path),
        detail: e.kind().to_string(),
    }
}

fn storage_err(path: &ScriptPath, e: StorageError) -> ScriptFileError {
    match e {
        StorageError::TooLarge { size, limit, .. } => ScriptFileError::TooLarge {
            name: shown(path),
            size,
            limit,
        },
        StorageError::WrongFileType { .. } => ScriptFileError::WrongType(shown(path)),
        other => ScriptFileError::Io {
            name: shown(path),
            detail: other.to_string(),
        },
    }
}

/// How [`ScriptStore::write`] treats the file already at the path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteMode {
    /// "Save as…", "New script": the file must not exist; created atomically without ever
    /// replacing one (`renameat2(RENAME_NOREPLACE)`, through `write_new_atomic`).
    CreateNew,
    /// "Save": replaces the file. With `Some(stamp)`, only while the file is still the one
    /// loaded or last saved (same size and modification time), else
    /// [`ScriptFileError::Changed`]; `None` overwrites whatever is there (the user confirmed).
    /// The check and the write are separate steps (a narrow race with another writer).
    Replace(Option<FileStamp>),
}

/// A script read from disk.
#[derive(Debug)]
pub struct LoadedScript {
    /// Its path.
    pub path: ScriptPath,
    /// The text as read (the editor sanitizes it).
    pub text: String,
    /// The file when it was read.
    pub stamp: FileStamp,
    /// The connection the script is linked to, if any.
    pub link: Option<ninesql_core::ConnectionId>,
}

/// The project's scripts folder.
#[derive(Debug, Clone)]
pub struct ScriptStore {
    dir: PathBuf,
    links_file: PathBuf,
    roots: DbeaverRoots,
}

impl ScriptStore {
    /// The scripts of a project, checked against `roots` (I4).
    #[must_use]
    pub fn new(project: &ProjectPaths, roots: DbeaverRoots) -> Self {
        Self {
            dir: project.scripts_dir(),
            links_file: project.script_links_file(),
            roots,
        }
    }

    /// The `scripts/` folder.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Checks `scripts/` (a real private directory, not in a DBeaver workspace) and returns
    /// its resolved path.
    fn checked_dir(&self, shown_as: &str) -> Result<PathBuf, ScriptFileError> {
        ensure_private_dir(&self.dir).map_err(|e| match e {
            StorageError::WrongFileType { .. } => ScriptFileError::Symlink(shown_as.to_owned()),
            other => ScriptFileError::Io {
                name: shown_as.to_owned(),
                detail: other.to_string(),
            },
        })?;
        let check = self.roots.check(&self.dir);
        match check.path {
            Some(resolved) if !check.inside => Ok(resolved),
            _ => Err(ScriptFileError::InsideDbeaver(shown_as.to_owned())),
        }
    }

    /// The absolute path of `script`, after the checks of the module documentation. With
    /// `create`, missing folders are created (`0700`).
    ///
    /// # Errors
    /// See [`ScriptFileError`].
    pub fn resolve(&self, script: &ScriptPath, create: bool) -> Result<PathBuf, ScriptFileError> {
        let name = shown(script);
        let root = self.checked_dir(&name)?;
        let components: Vec<&std::ffi::OsStr> = script
            .as_path()
            .components()
            .map(|c| match c {
                Component::Normal(n) => Ok(n),
                _ => Err(ScriptFileError::Outside(name.clone())),
            })
            .collect::<Result<_, _>>()?;
        let Some((file, folders)) = components.split_last() else {
            return Err(ScriptFileError::Outside(name));
        };
        let mut current = self.dir.clone();
        for folder in folders {
            current.push(folder);
            match fs::symlink_metadata(&current) {
                Ok(m) if m.file_type().is_symlink() => return Err(ScriptFileError::Symlink(name)),
                Ok(m) if !m.is_dir() => return Err(ScriptFileError::WrongType(name)),
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    if !create {
                        return Err(ScriptFileError::NotFound(name));
                    }
                    ensure_private_dir(&current).map_err(|e| storage_err(script, e))?;
                }
                Err(e) => return Err(io_err(script, &e)),
            }
        }
        // The parent, resolved, must still be under the resolved `scripts/`.
        let parent = fs::canonicalize(&current).map_err(|e| io_err(script, &e))?;
        if !parent.starts_with(&root) {
            return Err(ScriptFileError::Outside(name));
        }
        let path = current.join(file);
        match fs::symlink_metadata(&path) {
            Ok(m) if m.file_type().is_symlink() => return Err(ScriptFileError::Symlink(name)),
            Ok(m) if !m.is_file() => return Err(ScriptFileError::WrongType(name)),
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_err(script, &e)),
        }
        if self.roots.check(&path).inside {
            return Err(ScriptFileError::InsideDbeaver(name));
        }
        Ok(path)
    }

    /// The file's size and modification time, `None` when it does not exist.
    ///
    /// # Errors
    /// See [`Self::resolve`].
    pub fn stamp(&self, script: &ScriptPath) -> Result<Option<FileStamp>, ScriptFileError> {
        let path = match self.resolve(script, false) {
            Ok(p) => p,
            Err(ScriptFileError::NotFound(_)) => return Ok(None),
            Err(e) => return Err(e),
        };
        match fs::symlink_metadata(&path) {
            Ok(m) => Ok(Some(stamp_of(&m))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io_err(script, &e)),
        }
    }

    /// Reads a script and its link.
    ///
    /// # Errors
    /// See [`ScriptFileError`].
    pub fn read(&self, script: &ScriptPath) -> Result<LoadedScript, ScriptFileError> {
        let mut loaded = self.read_unlinked(script)?;
        // A missing or unreadable `scripts.json` only loses the link.
        loaded.link = self
            .load_links()
            .ok()
            .and_then(|l| l.connection_for(script));
        Ok(loaded)
    }

    /// Reads a script without looking up its link (`link` is `None`): the session restore
    /// reads many scripts and takes each tab's connection from the session file.
    ///
    /// # Errors
    /// See [`ScriptFileError`].
    pub fn read_unlinked(&self, script: &ScriptPath) -> Result<LoadedScript, ScriptFileError> {
        let path = self.resolve(script, false)?;
        let bytes = read_limited(&path, MAX_SCRIPT_BYTES)
            .map_err(|e| storage_err(script, e))?
            .ok_or_else(|| ScriptFileError::NotFound(shown(script)))?;
        let stamp = fs::symlink_metadata(&path)
            .map(|m| stamp_of(&m))
            .map_err(|e| io_err(script, &e))?;
        let text = String::from_utf8(bytes).map_err(|_| ScriptFileError::NotUtf8(shown(script)))?;
        Ok(LoadedScript {
            path: script.clone(),
            text,
            stamp,
            link: None,
        })
    }

    /// Writes a script atomically (`0600`) and records its link; see [`WriteMode`]. A failure
    /// to record the link does not undo the write: it is returned as `Ok((_, Some(problem)))`.
    ///
    /// # Errors
    /// See [`ScriptFileError`] ([`ScriptFileError::Exists`], [`ScriptFileError::Changed`]); the
    /// file is unchanged on error.
    pub fn write(
        &self,
        script: &ScriptPath,
        text: &str,
        link: Option<ninesql_core::ConnectionId>,
        mode: WriteMode,
    ) -> Result<(FileStamp, Option<String>), ScriptFileError> {
        let path = self.resolve(script, true)?;
        match mode {
            WriteMode::CreateNew => {
                write_new_atomic(&path, text.as_bytes()).map_err(|e| match e {
                    StorageError::Io { source, .. }
                        if source.kind() == io::ErrorKind::AlreadyExists =>
                    {
                        ScriptFileError::Exists(shown(script))
                    }
                    other => storage_err(script, other),
                })?;
            }
            WriteMode::Replace(expected) => {
                if let Some(expected) = expected {
                    let now = fs::symlink_metadata(&path)
                        .ok()
                        .filter(fs::Metadata::is_file)
                        .map(|m| stamp_of(&m));
                    if now != Some(expected) {
                        return Err(ScriptFileError::Changed(shown(script)));
                    }
                }
                write_atomic(&path, text.as_bytes()).map_err(|e| storage_err(script, e))?;
            }
        }
        let stamp = fs::symlink_metadata(&path)
            .map(|m| stamp_of(&m))
            .map_err(|e| io_err(script, &e))?;
        let problem = self
            .update_links(|links| links.set(script.clone(), link))
            .err()
            .map(|e| e.to_string());
        Ok((stamp, problem))
    }

    /// The first free `Script-<n>.sql` at the top of `scripts/`, from `start`.
    ///
    /// # Errors
    /// See [`ScriptFileError`].
    pub fn free_name(&self, start: usize) -> Result<ScriptPath, ScriptFileError> {
        for n in start.max(1)..start.max(1) + 10_000 {
            let candidate = ScriptPath::new(format!("Script-{n}.sql"))
                .map_err(|e| ScriptFileError::Outside(e.to_string()))?;
            if self.stamp(&candidate)?.is_none() {
                return Ok(candidate);
            }
        }
        Err(ScriptFileError::Exists("Script-<n>.sql".to_owned()))
    }

    /// Every script under `scripts/`, sorted, bounded ([`MAX_LISTED`], [`MAX_LIST_ENTRIES`]).
    /// Returns whether the list was cut.
    ///
    /// # Errors
    /// The checks of `scripts/` itself.
    pub fn list(&self) -> Result<(Vec<ScriptPath>, bool), ScriptFileError> {
        self.checked_dir("scripts")?;
        let mut out = Vec::new();
        let mut seen = 0usize;
        let mut truncated = false;
        let mut stack = vec![(self.dir.clone(), PathBuf::new())];
        while let Some((dir, rel)) = stack.pop() {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                seen += 1;
                if seen > MAX_LIST_ENTRIES || out.len() >= MAX_LISTED {
                    truncated = true;
                    break;
                }
                let name = entry.file_name();
                if name.to_string_lossy().starts_with('.') {
                    continue;
                }
                // `file_type` does not follow symbolic links: they are skipped.
                let Ok(kind) = entry.file_type() else {
                    continue;
                };
                let child = rel.join(&name);
                if kind.is_dir() {
                    if child.components().count() < MAX_SCRIPT_PATH_DEPTH {
                        stack.push((entry.path(), child));
                    }
                } else if kind.is_file()
                    && let Ok(script) = ScriptPath::new(child)
                {
                    out.push(script);
                }
            }
            if truncated {
                break;
            }
        }
        out.sort();
        Ok((out, truncated))
    }

    /// `scripts.json`, empty when missing.
    ///
    /// # Errors
    /// [`ScriptFileError::Links`]: unreadable, invalid or newer.
    pub fn load_links(&self) -> Result<ScriptLinks, ScriptFileError> {
        let bytes = read_private(&self.links_file, MAX_LINKS_BYTES)
            .map_err(|e| ScriptFileError::Links(e.to_string()))?;
        let Some(bytes) = bytes else {
            return Ok(ScriptLinks::new());
        };
        let text =
            String::from_utf8(bytes).map_err(|_| ScriptFileError::Links("not UTF-8".to_owned()))?;
        ScriptLinks::from_json(&text).map_err(|e| ScriptFileError::Links(e.to_string()))
    }

    /// Reads, changes and writes `scripts.json` (serialized within the process). A newer or
    /// invalid file is never overwritten.
    ///
    /// # Errors
    /// [`ScriptFileError::Links`].
    pub fn update_links(&self, f: impl FnOnce(&mut ScriptLinks)) -> Result<(), ScriptFileError> {
        let _guard = LINKS_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        let mut links = self.load_links()?;
        let before = links.clone();
        f(&mut links);
        if links == before {
            return Ok(());
        }
        let json = links
            .to_json()
            .map_err(|e| ScriptFileError::Links(e.to_string()))?;
        write_atomic(&self.links_file, json.as_bytes())
            .map_err(|e| ScriptFileError::Links(e.to_string()))
    }
}

/// Size and modification time of a file.
#[must_use]
pub fn stamp_of(meta: &fs::Metadata) -> FileStamp {
    let (secs, nanos) = match meta.modified().map(|t| t.duration_since(UNIX_EPOCH)) {
        Ok(Ok(d)) => (
            i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
            d.subsec_nanos(),
        ),
        // Before the epoch, or no modification time: the raw fields.
        _ => (
            meta.mtime(),
            u32::try_from(meta.mtime_nsec()).unwrap_or_default(),
        ),
    };
    FileStamp {
        len: meta.len(),
        modified_secs: secs,
        modified_nanos: nanos,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use ninesql_core::ConnectionId;
    use ninesql_core::storage::{Paths, ProjectName, XdgDirs};

    use super::*;

    fn store(base: &Path, roots: DbeaverRoots) -> ScriptStore {
        let base = base.to_path_buf();
        let xdg = XdgDirs::from_lookup(|k| {
            let dir = match k {
                "XDG_CONFIG_HOME" => "config",
                "XDG_DATA_HOME" => "data",
                "XDG_STATE_HOME" => "state",
                "HOME" => "home",
                _ => return None,
            };
            Some(base.join(dir).into_os_string())
        })
        .unwrap();
        let project = Paths::new(&xdg).project(&ProjectName::default_project());
        ScriptStore::new(&project, roots)
    }

    fn sp(p: &str) -> ScriptPath {
        ScriptPath::new(p).unwrap()
    }

    #[test]
    fn save_then_open_round_trip_with_folders_and_links() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path(), DbeaverRoots::none());
        let c = ConnectionId::new_random();
        let (stamp, problem) = s
            .write(
                &sp("reports/monthly.sql"),
                "SELECT 1;\n",
                Some(c),
                WriteMode::CreateNew,
            )
            .unwrap();
        assert!(problem.is_none());
        assert_eq!(stamp.len, 10);
        let file = s.dir().join("reports/monthly.sql");
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&file), 0o600);
        assert_eq!(mode(&s.dir().join("reports")), 0o700);
        let loaded = s.read(&sp("reports/monthly.sql")).unwrap();
        assert_eq!(loaded.text, "SELECT 1;\n");
        assert_eq!(loaded.link, Some(c));
        assert_eq!(loaded.stamp, stamp);
        // "Save as" refuses a taken name; "Save" overwrites.
        assert!(matches!(
            s.write(&sp("reports/monthly.sql"), "x", None, WriteMode::CreateNew),
            Err(ScriptFileError::Exists(_))
        ));
        s.write(
            &sp("reports/monthly.sql"),
            "SELECT 2;",
            None,
            WriteMode::Replace(None),
        )
        .unwrap();
        let loaded = s.read(&sp("reports/monthly.sql")).unwrap();
        assert_eq!(loaded.text, "SELECT 2;");
        // "Save" checked against the stamp: refused once the file changed, allowed when not.
        assert!(matches!(
            s.write(
                &sp("reports/monthly.sql"),
                "x",
                None,
                WriteMode::Replace(Some(stamp))
            ),
            Err(ScriptFileError::Changed(_))
        ));
        let (now, _) = s
            .write(
                &sp("reports/monthly.sql"),
                "SELECT 2;",
                None,
                WriteMode::Replace(Some(loaded.stamp)),
            )
            .unwrap();
        assert!(matches!(
            s.write(&sp("gone.sql"), "x", None, WriteMode::Replace(Some(now))),
            Err(ScriptFileError::Changed(_))
        ));
        assert_eq!(loaded.link, None, "unlinked on save without a connection");
        let (list, cut) = s.list().unwrap();
        assert_eq!(list, vec![sp("reports/monthly.sql")]);
        assert!(!cut);
        // No temp file left behind.
        let names: Vec<_> = fs::read_dir(s.dir().join("reports"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 1);
        assert_eq!(s.free_name(1).unwrap(), sp("Script-1.sql"));
        s.write(&sp("Script-1.sql"), "", None, WriteMode::CreateNew)
            .unwrap();
        assert_eq!(s.free_name(1).unwrap(), sp("Script-2.sql"));
    }

    #[test]
    fn symlink_escapes_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path(), DbeaverRoots::none());
        ensure_private_dir(s.dir()).unwrap();
        let outside = tmp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("x.sql"), "SELECT 'outside';").unwrap();
        // A folder link and a file link inside scripts/.
        std::os::unix::fs::symlink(&outside, s.dir().join("evil")).unwrap();
        std::os::unix::fs::symlink(outside.join("x.sql"), s.dir().join("link.sql")).unwrap();
        for p in ["evil/x.sql", "link.sql"] {
            assert!(
                matches!(s.read(&sp(p)), Err(ScriptFileError::Symlink(_))),
                "{p}"
            );
            assert!(
                matches!(
                    s.write(&sp(p), "DROP TABLE t;", None, WriteMode::Replace(None)),
                    Err(ScriptFileError::Symlink(_))
                ),
                "{p}"
            );
        }
        assert_eq!(
            fs::read_to_string(outside.join("x.sql")).unwrap(),
            "SELECT 'outside';",
            "nothing written through a link"
        );
        assert!(!outside.join("new.sql").exists());
        assert!(
            s.write(&sp("evil/new.sql"), "x", None, WriteMode::CreateNew)
                .is_err()
        );
        assert!(!outside.join("new.sql").exists());
        // Links are not listed.
        assert!(s.list().unwrap().0.is_empty());
        // A scripts/ folder that is itself a link is refused.
        let tmp2 = tempfile::tempdir().unwrap();
        let s2 = store(tmp2.path(), DbeaverRoots::none());
        fs::create_dir_all(s2.dir().parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&outside, s2.dir()).unwrap();
        assert!(s2.read(&sp("x.sql")).is_err());
        assert!(
            s2.write(&sp("y.sql"), "x", None, WriteMode::CreateNew)
                .is_err()
        );
        assert!(!outside.join("y.sql").exists());
    }

    #[test]
    fn fifos_are_refused_without_blocking() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path(), DbeaverRoots::none());
        ensure_private_dir(s.dir()).unwrap();
        let fifo = s.dir().join("pipe.sql");
        let made = std::process::Command::new("mkfifo").arg(&fifo).status();
        if made.is_ok_and(|st| st.success()) {
            assert!(matches!(
                s.read(&sp("pipe.sql")),
                Err(ScriptFileError::WrongType(_))
            ));
        }
    }

    #[test]
    fn dbeaver_workspaces_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = DbeaverRoots::none();
        // The whole NineSQL data folder counts as a DBeaver root here.
        roots.add(tmp.path().join("data"));
        let s = store(tmp.path(), roots);
        assert!(matches!(
            s.write(&sp("a.sql"), "SELECT 1;", None, WriteMode::CreateNew),
            Err(ScriptFileError::InsideDbeaver(_))
        ));
        assert!(!s.dir().join("a.sql").exists());
        fs::write(s.dir().join("b.sql"), "SELECT 1;").unwrap();
        assert!(matches!(
            s.read(&sp("b.sql")),
            Err(ScriptFileError::InsideDbeaver(_))
        ));
        assert!(matches!(s.list(), Err(ScriptFileError::InsideDbeaver(_))));
        // A `DBeaverData` component anywhere counts too.
        let tmp2 = tempfile::tempdir().unwrap();
        let s2 = store(&tmp2.path().join("DBeaverData"), DbeaverRoots::none());
        assert!(matches!(
            s2.write(&sp("a.sql"), "SELECT 1;", None, WriteMode::CreateNew),
            Err(ScriptFileError::InsideDbeaver(_))
        ));
    }

    #[test]
    fn size_cap_and_encoding_on_open() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path(), DbeaverRoots::none());
        ensure_private_dir(s.dir()).unwrap();
        let big = s.dir().join("big.sql");
        let f = fs::File::create(&big).unwrap();
        f.set_len(MAX_SCRIPT_BYTES + 1).unwrap();
        assert!(matches!(
            s.read(&sp("big.sql")),
            Err(ScriptFileError::TooLarge { .. })
        ));
        let at = s.dir().join("at-limit.sql");
        fs::File::create(&at)
            .unwrap()
            .set_len(MAX_SCRIPT_BYTES)
            .unwrap();
        assert!(s.read(&sp("at-limit.sql")).is_ok());
        fs::write(s.dir().join("latin1.sql"), b"SELECT '\xe9';").unwrap();
        assert!(matches!(
            s.read(&sp("latin1.sql")),
            Err(ScriptFileError::NotUtf8(_))
        ));
        assert!(matches!(
            s.read(&sp("missing.sql")),
            Err(ScriptFileError::NotFound(_))
        ));
        assert!(matches!(
            s.read(&sp("no/such/dir.sql")),
            Err(ScriptFileError::NotFound(_))
        ));
    }

    #[test]
    fn newer_links_file_is_never_overwritten() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path(), DbeaverRoots::none());
        ensure_private_dir(s.dir()).unwrap();
        let newer = r#"{"schema_version": 99, "links": {}}"#;
        fs::write(&s.links_file, newer).unwrap();
        let (_, problem) = s
            .write(
                &sp("a.sql"),
                "SELECT 1;",
                Some(ConnectionId::new_random()),
                WriteMode::CreateNew,
            )
            .unwrap();
        assert!(problem.is_some_and(|p| p.contains("newer")));
        assert_eq!(fs::read_to_string(&s.links_file).unwrap(), newer);
        assert!(s.read(&sp("a.sql")).unwrap().link.is_none());
    }
}
