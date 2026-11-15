//! The only filesystem access of this crate, and it is read-only (I4).
//!
//! - Files are opened with [`File::open`] (`O_RDONLY`): never created, truncated, written,
//!   renamed, deleted or locked. No `OpenOptions`, no `fs::write`, no advisory lock.
//! - Only regular files are opened (checked before opening, so a FIFO cannot block the import).
//! - Reads are bounded; the bytes are returned in a zeroizing buffer because DBeaver files can hold
//!   credentials (I1).
//!
//! A test (`tests/read_only.rs`) scans the sources of the crate to keep it that way.

use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use zeroize::Zeroizing;

use crate::error::DbeaverError;
use crate::source::CancelFlag;

/// Upper bound for `data-sources*.json` (a workspace with thousands of connections is < 10 MiB).
pub(crate) const MAX_DATA_SOURCES_BYTES: u64 = 32 * 1024 * 1024;
/// Upper bound for `credentials-config.json`.
pub(crate) const MAX_CREDENTIALS_BYTES: u64 = 8 * 1024 * 1024;
/// Upper bound for small text files (`dbeaver-workspace.properties`).
pub(crate) const MAX_SMALL_FILE_BYTES: u64 = 64 * 1024;
/// Upper bound for the entries kept from one directory (after sorting by name).
pub(crate) const MAX_DIR_ENTRIES: usize = 10_000;
/// Upper bound for the names read from one directory before sorting and cutting.
pub(crate) const MAX_DIR_SCAN: usize = 100_000;
/// Names read between two checks of the cancel flag while listing a directory.
const CANCEL_EVERY_NAMES: usize = 1024;

/// Pseudo-filesystems whose files report a size of 0 or block: never read.
const SPECIAL_ROOTS: [&str; 3] = ["/proc", "/sys", "/dev"];

/// Identity of a file or directory: device and inode (symlinks followed).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct FileId {
    dev: u64,
    ino: u64,
}

fn id_of(meta: &fs::Metadata) -> FileId {
    use std::os::unix::fs::MetadataExt;
    FileId {
        dev: meta.dev(),
        ino: meta.ino(),
    }
}

/// Identity of an existing path (symlinks followed); `None` when it does not resolve.
pub(crate) fn file_id(path: &Path) -> Option<FileId> {
    fs::metadata(path).ok().map(|m| id_of(&m))
}

/// What one detection may still examine: directories listed and names read or `stat`ed, shared
/// by every listing of the detection, plus its cancel flag.
pub(crate) struct ScanBudget<'a> {
    dirs: usize,
    names: usize,
    cancel: Option<&'a CancelFlag>,
    /// A limit was reached: some listing was cut.
    pub(crate) exhausted: bool,
}

impl<'a> ScanBudget<'a> {
    pub(crate) fn new(dirs: usize, names: usize, cancel: Option<&'a CancelFlag>) -> Self {
        Self {
            dirs,
            names,
            cancel,
            exhausted: false,
        }
    }

    pub(crate) fn check_cancel(&self) -> Result<(), DbeaverError> {
        if self.cancel.is_some_and(CancelFlag::is_cancelled) {
            Err(DbeaverError::Cancelled)
        } else {
            Ok(())
        }
    }

    /// Takes one name (a directory entry read or a `stat`); `false` once the budget is spent.
    fn take_name(&mut self) -> bool {
        if self.names == 0 {
            self.exhausted = true;
            return false;
        }
        self.names -= 1;
        true
    }

    fn take_dir(&mut self) -> bool {
        if self.dirs == 0 {
            self.exhausted = true;
            return false;
        }
        self.dirs -= 1;
        true
    }
}

/// Reads a whole regular file, read-only, refusing files over `limit` bytes.
pub(crate) fn read_limited(path: &Path, limit: u64) -> Result<Zeroizing<Vec<u8>>, DbeaverError> {
    read_counted(path, limit).0
}

/// [`read_limited`], with the bytes to charge to a read budget, on success **and** on failure:
/// the bytes actually read, or `limit` for a file that reports a size of 0 but has content (a
/// pseudo-file). Files under `/proc`, `/sys` and `/dev` are refused without being opened.
pub(crate) fn read_counted(
    path: &Path,
    limit: u64,
) -> (Result<Zeroizing<Vec<u8>>, DbeaverError>, u64) {
    if canonicalize(path).is_some_and(|c| SPECIAL_ROOTS.iter().any(|r| c.starts_with(r))) {
        return (
            Err(DbeaverError::SpecialFilesystem {
                path: path.to_path_buf(),
            }),
            0,
        );
    }
    let opened = (|| {
        let meta = fs::metadata(path).map_err(|e| DbeaverError::io(path, &e))?;
        if !meta.is_file() {
            return Err(DbeaverError::NotAFile {
                path: path.to_path_buf(),
            });
        }
        if meta.len() > limit {
            return Err(DbeaverError::TooLarge {
                path: path.to_path_buf(),
                limit,
            });
        }
        let file = File::open(path).map_err(|e| DbeaverError::io(path, &e))?;
        // Re-check on the opened file: the path may have been swapped meanwhile (FIFO, device…).
        let meta = file.metadata().map_err(|e| DbeaverError::io(path, &e))?;
        if !meta.is_file() {
            return Err(DbeaverError::NotAFile {
                path: path.to_path_buf(),
            });
        }
        if meta.len() > limit {
            return Err(DbeaverError::TooLarge {
                path: path.to_path_buf(),
                limit,
            });
        }
        Ok((file, meta.len()))
    })();
    match opened {
        Ok((file, expected)) => read_bounded(file, expected, limit, path),
        Err(e) => (Err(e), 0),
    }
}

/// Reads at most `expected + 1` bytes: a file that turns out longer than its size when opened
/// grew meanwhile (or is a pseudo-file) and is refused. Returns the bytes to charge.
fn read_bounded<R: Read>(
    reader: R,
    expected: u64,
    limit: u64,
    path: &Path,
) -> (Result<Zeroizing<Vec<u8>>, DbeaverError>, u64) {
    // Pre-size the buffer so it is not reallocated (a reallocation would leave an unzeroized
    // copy of the content behind). The `take` bound protects against a file growing meanwhile.
    let capacity = usize::try_from(expected.saturating_add(1)).unwrap_or(0);
    let mut buf = Zeroizing::new(Vec::with_capacity(capacity));
    let res = reader
        .take(expected.saturating_add(1))
        .read_to_end(&mut buf);
    // `read_to_end` keeps what it read before an error: it is charged too.
    let read = buf.len() as u64;
    if let Err(e) = res {
        return (Err(DbeaverError::io(path, &e)), read);
    }
    if read > expected {
        if expected == 0 {
            // Reports a size of 0 but has content: a pseudo-file. Charged in full, so links to
            // such files exhaust the budget instead of being read over and over.
            return (
                Err(DbeaverError::NotAFile {
                    path: path.to_path_buf(),
                }),
                limit.max(read),
            );
        }
        return (
            Err(DbeaverError::Changed {
                path: path.to_path_buf(),
            }),
            read,
        );
    }
    (Ok(buf), read)
}

/// One directory entry: its path, name, type and identity (symlinks followed).
pub(crate) struct Entry {
    pub(crate) path: PathBuf,
    pub(crate) name: String,
    pub(crate) is_dir: bool,
    pub(crate) is_file: bool,
}

/// A directory listing and whether it was cut.
pub(crate) struct Listing {
    pub(crate) entries: Vec<Entry>,
    /// Some entries were not listed: more than [`MAX_DIR_SCAN`] names, more than
    /// [`MAX_DIR_ENTRIES`] kept, or the scan budget ran out.
    pub(crate) truncated: bool,
}

/// Lists the entries of a directory whose name passes `keep`, sorted by name: at most
/// [`MAX_DIR_SCAN`] names are read, the kept names are sorted, the first [`MAX_DIR_ENTRIES`]
/// are kept and only those are `stat`ed. Names that are not valid UTF-8 are skipped. Every name
/// read and every `stat` draws on `budget`.
///
/// # Errors
/// [`DbeaverError::Io`] when the directory cannot be listed, [`DbeaverError::Cancelled`].
pub(crate) fn list_dir(
    dir: &Path,
    keep: impl Fn(&str) -> bool,
    budget: &mut ScanBudget<'_>,
) -> Result<Listing, DbeaverError> {
    budget.check_cancel()?;
    let mut truncated = false;
    if !budget.take_dir() {
        return Ok(Listing {
            entries: Vec::new(),
            truncated: true,
        });
    }
    let mut names: Vec<(String, PathBuf)> = Vec::new();
    for (scanned, entry) in fs::read_dir(dir)
        .map_err(|e| DbeaverError::io(dir, &e))?
        .enumerate()
    {
        if scanned % CANCEL_EVERY_NAMES == 0 {
            budget.check_cancel()?;
        }
        if scanned >= MAX_DIR_SCAN || !budget.take_name() {
            truncated = true;
            break;
        }
        let Ok(entry) = entry else { continue };
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if keep(&name) {
            names.push((name, entry.path()));
        }
    }
    names.sort_by(|a, b| a.0.cmp(&b.0));
    if names.len() > MAX_DIR_ENTRIES {
        names.truncate(MAX_DIR_ENTRIES);
        truncated = true;
    }
    let mut entries = Vec::with_capacity(names.len());
    for (name, path) in names {
        if !budget.take_name() {
            truncated = true;
            break;
        }
        // `fs::metadata` follows symlinks; a broken link is neither a file nor a directory.
        let (is_dir, is_file) = fs::metadata(&path)
            .map(|m| (m.is_dir(), m.is_file()))
            .unwrap_or((false, false));
        entries.push(Entry {
            path,
            name,
            is_dir,
            is_file,
        });
    }
    if truncated {
        tracing::warn!(dir = %crate::report::shown_path(dir), "directory listing truncated");
    }
    Ok(Listing { entries, truncated })
}

/// Identity of `path` if it is a directory, drawing one name on `budget`; `None` when it is not
/// a directory, does not resolve or the budget is spent (`budget.exhausted` tells).
pub(crate) fn dir_id(path: &Path, budget: &mut ScanBudget<'_>) -> Option<FileId> {
    if !budget.take_name() {
        return None;
    }
    fs::metadata(path)
        .ok()
        .filter(fs::Metadata::is_dir)
        .map(|m| id_of(&m))
}

/// Canonical form of an existing path (symlinks resolved); `None` when it does not resolve.
/// Read-only: only `stat` / `readlink`.
pub(crate) fn canonicalize(path: &Path) -> Option<PathBuf> {
    fs::canonicalize(path).ok()
}

/// Whether `path` is an existing directory (symlinks followed).
pub(crate) fn is_dir(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_dir())
}

/// Whether `path` is an existing regular file (symlinks followed).
pub(crate) fn is_file(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Yields `data` then fails, like a file whose device errors partway.
    struct FailAfter<'a>(&'a [u8]);

    impl Read for FailAfter<'_> {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            if self.0.is_empty() {
                return Err(std::io::Error::other("device error"));
            }
            let n = out.len().min(self.0.len());
            out[..n].copy_from_slice(&self.0[..n]);
            self.0 = &self.0[n..];
            Ok(n)
        }
    }

    #[test]
    fn a_read_failing_partway_is_charged() {
        let (res, charged) = read_bounded(FailAfter(b"0123456789"), 100, 1000, Path::new("x"));
        assert!(matches!(res, Err(DbeaverError::Io { .. })));
        assert_eq!(charged, 10);
    }

    #[test]
    fn a_file_that_grew_is_refused_and_charged() {
        let data = [b'a'; 64];
        let (res, charged) = read_bounded(&data[..], 10, 1000, Path::new("x"));
        assert!(matches!(res, Err(DbeaverError::Changed { .. })));
        assert_eq!(charged, 11, "at most one byte past the size is read");
    }

    #[test]
    fn a_pseudo_file_is_charged_the_full_limit() {
        let data = [b'a'; 64];
        let (res, charged) = read_bounded(&data[..], 0, 1000, Path::new("x"));
        assert!(matches!(res, Err(DbeaverError::NotAFile { .. })));
        assert_eq!(charged, 1000);
        // A real empty file costs nothing.
        let (res, charged) = read_bounded(&[][..], 0, 1000, Path::new("x"));
        assert!(res.is_ok_and(|b| b.is_empty()));
        assert_eq!(charged, 0);
    }

    #[test]
    fn special_filesystems_are_not_read() {
        for p in ["/proc/self/status", "/proc/self/pagemap", "/dev/zero"] {
            let (res, charged) = read_counted(Path::new(p), 1 << 20);
            assert!(
                matches!(res, Err(DbeaverError::SpecialFilesystem { .. })),
                "{p}"
            );
            let message = res.err().map(|e| e.to_string()).unwrap_or_default();
            assert!(message.contains("special filesystem"), "{message}");
            assert_eq!(charged, 0);
        }
    }
}
