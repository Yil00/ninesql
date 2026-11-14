//! File system primitives: private directories, atomic writes, bounded reads (ADR-0008 §3).

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::StorageError;

/// Mode of every directory NineSQL creates.
pub const DIR_MODE: u32 = 0o700;
/// Mode of every file NineSQL writes.
pub const FILE_MODE: u32 = 0o600;
/// Default size limit when reading a workspace file (64 MiB).
pub const DEFAULT_READ_LIMIT: u64 = 64 * 1024 * 1024;

/// Group and other permission bits.
const GROUP_OTHER: u32 = 0o077;

/// Creates `dir` and its missing parents with mode `0700`. If `dir` already exists with group or
/// other permissions, they are removed (a warning is logged). Existing parents are left alone.
///
/// # Errors
/// I/O errors, or [`StorageError::WrongFileType`] if `dir` exists and is not a directory.
pub fn ensure_private_dir(dir: &Path) -> Result<(), StorageError> {
    DirBuilder::new()
        .recursive(true)
        .mode(DIR_MODE)
        .create(dir)
        .map_err(|e| StorageError::io(dir, e))?;
    let meta = fs::metadata(dir).map_err(|e| StorageError::io(dir, e))?;
    if !meta.is_dir() {
        return Err(StorageError::WrongFileType {
            path: dir.to_path_buf(),
            expected: "directory",
        });
    }
    restrict_mode(dir, meta.permissions().mode(), DIR_MODE)
}

/// Removes group / other bits from an existing NineSQL file, if any (a warning is logged).
///
/// # Errors
/// I/O errors.
pub fn ensure_private_file(file: &Path) -> Result<(), StorageError> {
    let meta = fs::metadata(file).map_err(|e| StorageError::io(file, e))?;
    restrict_mode(file, meta.permissions().mode(), FILE_MODE)
}

fn restrict_mode(path: &Path, current: u32, wanted: u32) -> Result<(), StorageError> {
    if current & GROUP_OTHER != 0 {
        tracing::warn!(
            path = %path.display(),
            mode = format_args!("{:o}", current & 0o7777),
            "tightening permissions of a NineSQL path"
        );
        fs::set_permissions(path, fs::Permissions::from_mode(wanted))
            .map_err(|e| StorageError::io(path, e))?;
    }
    Ok(())
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Writes `bytes` to `path` atomically: a temp file (`0600`) is created next to `path`, written,
/// `fsync`ed and renamed over `path`, then the directory is `fsync`ed. Readers see either the old
/// or the new content, never a mix, even after a crash. The parent directory must exist.
///
/// # Errors
/// I/O errors. On error the temp file is removed and `path` is unchanged.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), StorageError> {
    let dir = parent_dir(path);
    let (tmp_path, mut tmp) = create_temp(path, dir)?;
    let result = (|| {
        tmp.write_all(bytes)?;
        tmp.sync_all()?;
        drop(tmp);
        fs::rename(&tmp_path, path)
    })();
    if let Err(e) = result {
        // Best effort: the temp file is ours and not referenced anywhere.
        let _ = fs::remove_file(&tmp_path);
        return Err(StorageError::io(path, e));
    }
    sync_dir(dir)
}

fn parent_dir(path: &Path) -> &Path {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    }
}

fn create_temp(path: &Path, dir: &Path) -> Result<(PathBuf, File), StorageError> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let pid = std::process::id();
    let mut last_err = None;
    for _ in 0..16 {
        let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let tmp_path = dir.join(format!(".{name}.{pid}-{n}-{nanos}.tmp"));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(FILE_MODE)
            .open(&tmp_path)
        {
            Ok(file) => return Ok((tmp_path, file)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => last_err = Some(e),
            Err(e) => return Err(StorageError::io(tmp_path, e)),
        }
    }
    Err(StorageError::io(
        path,
        last_err.unwrap_or_else(|| io::Error::other("cannot create a temp file")),
    ))
}

fn sync_dir(dir: &Path) -> Result<(), StorageError> {
    File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(|e| StorageError::io(dir, e))
}

/// Reads a whole file, refusing files larger than `limit` bytes. Returns `None` if the file does
/// not exist.
///
/// # Errors
/// I/O errors, [`StorageError::WrongFileType`] for a non-regular file,
/// [`StorageError::TooLarge`].
pub fn read_limited(path: &Path, limit: u64) -> Result<Option<Vec<u8>>, StorageError> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(StorageError::io(path, e)),
    };
    let meta = file.metadata().map_err(|e| StorageError::io(path, e))?;
    if !meta.is_file() {
        return Err(StorageError::WrongFileType {
            path: path.to_path_buf(),
            expected: "regular file",
        });
    }
    if meta.len() > limit {
        return Err(StorageError::TooLarge {
            path: path.to_path_buf(),
            size: meta.len(),
            limit,
        });
    }
    let mut buf = Vec::new();
    // `take` guards against a file growing between `metadata` and `read`.
    file.take(limit.saturating_add(1))
        .read_to_end(&mut buf)
        .map_err(|e| StorageError::io(path, e))?;
    if buf.len() as u64 > limit {
        return Err(StorageError::TooLarge {
            path: path.to_path_buf(),
            size: buf.len() as u64,
            limit,
        });
    }
    Ok(Some(buf))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn mode(p: &Path) -> u32 {
        fs::metadata(p).unwrap().permissions().mode() & 0o7777
    }

    #[test]
    fn creates_private_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("a/b/c");
        ensure_private_dir(&dir).unwrap();
        for p in [tmp.path().join("a"), tmp.path().join("a/b"), dir.clone()] {
            assert_eq!(mode(&p), DIR_MODE, "{}", p.display());
        }
    }

    #[test]
    fn tightens_existing_directory_and_file() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("open");
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        ensure_private_dir(&dir).unwrap();
        assert_eq!(mode(&dir), DIR_MODE);

        let file = dir.join("f");
        fs::write(&file, b"x").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        ensure_private_file(&file).unwrap();
        assert_eq!(mode(&file), FILE_MODE);
    }

    #[test]
    fn file_where_directory_expected() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("f");
        fs::write(&file, b"x").unwrap();
        assert!(ensure_private_dir(&file).is_err());
    }

    #[test]
    fn atomic_write_replaces_content_and_leaves_no_temp() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("doc.json");
        write_atomic(&path, b"first").unwrap();
        assert_eq!(mode(&path), FILE_MODE);
        // A pre-existing loose file is replaced by a 0600 one.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        write_atomic(&path, b"second").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"second");
        assert_eq!(mode(&path), FILE_MODE);
        let names: Vec<_> = fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("doc.json")]);
    }

    #[test]
    fn atomic_write_failure_keeps_old_content() {
        let tmp = tempfile::tempdir().unwrap();
        // Target is a non-empty directory: rename fails, nothing is left behind.
        let target = tmp.path().join("target");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("keep"), b"k").unwrap();
        assert!(write_atomic(&target, b"data").is_err());
        assert!(target.join("keep").exists());
        let count = fs::read_dir(tmp.path()).unwrap().count();
        assert_eq!(count, 1);
    }

    #[test]
    fn read_limits() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("f");
        assert!(read_limited(&path, 10).unwrap().is_none());
        fs::write(&path, b"0123456789").unwrap();
        assert_eq!(read_limited(&path, 10).unwrap().unwrap().len(), 10);
        assert!(matches!(
            read_limited(&path, 9),
            Err(StorageError::TooLarge { .. })
        ));
        assert!(matches!(
            read_limited(tmp.path(), 10),
            Err(StorageError::WrongFileType { .. }) | Err(StorageError::Io { .. })
        ));
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(32))]
        #[test]
        fn atomic_write_round_trip(data in proptest::collection::vec(any::<u8>(), 0..4096)) {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("blob");
            write_atomic(&path, &data).unwrap();
            prop_assert_eq!(read_limited(&path, DEFAULT_READ_LIMIT).unwrap().unwrap(), data);
        }
    }
}
