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

/// Upper bound for `data-sources*.json` (a workspace with thousands of connections is < 10 MiB).
pub(crate) const MAX_DATA_SOURCES_BYTES: u64 = 32 * 1024 * 1024;
/// Upper bound for `credentials-config.json`.
pub(crate) const MAX_CREDENTIALS_BYTES: u64 = 8 * 1024 * 1024;
/// Upper bound for small text files (`dbeaver-workspace.properties`).
pub(crate) const MAX_SMALL_FILE_BYTES: u64 = 64 * 1024;
/// Upper bound for the entries read from one directory.
pub(crate) const MAX_DIR_ENTRIES: usize = 10_000;

/// Reads a whole regular file, read-only, refusing files over `limit` bytes.
pub(crate) fn read_limited(path: &Path, limit: u64) -> Result<Zeroizing<Vec<u8>>, DbeaverError> {
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
    // Pre-size the buffer so it is not reallocated (a reallocation would leave an unzeroized
    // copy of the content behind). The `take` bound protects against a file growing meanwhile.
    let capacity = usize::try_from(meta.len().saturating_add(1)).unwrap_or(0);
    let mut buf = Zeroizing::new(Vec::with_capacity(capacity));
    file.take(limit.saturating_add(1))
        .read_to_end(&mut buf)
        .map_err(|e| DbeaverError::io(path, &e))?;
    if buf.len() as u64 > limit {
        return Err(DbeaverError::TooLarge {
            path: path.to_path_buf(),
            limit,
        });
    }
    Ok(buf)
}

/// One directory entry: its path, name and whether it is a directory (symlinks followed).
pub(crate) struct Entry {
    pub(crate) path: PathBuf,
    pub(crate) name: String,
    pub(crate) is_dir: bool,
    pub(crate) is_file: bool,
}

/// Lists a directory (at most [`MAX_DIR_ENTRIES`] entries, sorted by name). Entries whose name is
/// not valid UTF-8 are skipped.
pub(crate) fn list_dir(dir: &Path) -> Result<Vec<Entry>, DbeaverError> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir).map_err(|e| DbeaverError::io(dir, &e))? {
        if out.len() >= MAX_DIR_ENTRIES {
            tracing::warn!(dir = %dir.display(), "directory listing truncated");
            break;
        }
        let Ok(entry) = entry else { continue };
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        let path = entry.path();
        // `fs::metadata` follows symlinks; a broken link is neither a file nor a directory.
        let (is_dir, is_file) = fs::metadata(&path)
            .map(|m| (m.is_dir(), m.is_file()))
            .unwrap_or((false, false));
        out.push(Entry {
            path,
            name,
            is_dir,
            is_file,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Whether `path` is an existing directory (symlinks followed).
pub(crate) fn is_dir(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_dir())
}

/// Whether `path` is an existing regular file (symlinks followed).
pub(crate) fn is_file(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_file())
}
