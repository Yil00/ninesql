//! File system primitives: private directories, atomic writes, bounded reads (ADR-0008 §3).

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
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

/// The last component of `path`, for log events: a full path names the user's home directory
/// (their account name), so `warn` / `error` / `info` events carry only the file or directory
/// name (ADR-0029 §3.2). Control characters are left for the subscriber to escape.
#[must_use]
pub fn log_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || String::from("(no file name)"),
        |n| n.to_string_lossy().into_owned(),
    )
}

/// Creates `dir` and its missing parents with mode `0700`, then checks `dir` itself:
///
/// - it must be a real directory: a symbolic link in place of `dir` is refused, never followed
///   (the check and every later step go through a descriptor opened with `O_NOFOLLOW`);
/// - it must be owned by the current (effective) user, otherwise [`StorageError::NotOwned`];
/// - group / other permission bits are removed through that descriptor (a warning is logged).
///
/// Existing parents are left alone and may be symbolic links (a symlinked `$HOME` is common):
/// callers check each NineSQL directory of a path in turn, outermost first.
///
/// # Errors
/// I/O errors, [`StorageError::WrongFileType`] if `dir` exists and is not a directory (or is a
/// symbolic link), [`StorageError::NotOwned`].
pub fn ensure_private_dir(dir: &Path) -> Result<(), StorageError> {
    match fs::symlink_metadata(dir) {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => DirBuilder::new()
            .recursive(true)
            .mode(DIR_MODE)
            .create(dir)
            .map_err(|e| StorageError::io(dir, e))?,
        Err(e) => return Err(StorageError::io(dir, e)),
    }
    let handle = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(dir)
    {
        Ok(handle) => handle,
        Err(e) if matches!(e.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR)) => {
            return Err(not_a_directory(dir));
        }
        Err(e) => return Err(StorageError::io(dir, e)),
    };
    let meta = handle.metadata().map_err(|e| StorageError::io(dir, e))?;
    if !meta.is_dir() {
        return Err(not_a_directory(dir));
    }
    check_owner(dir, meta.uid(), current_uid())?;
    let mode = meta.permissions().mode();
    if mode & GROUP_OTHER != 0 {
        tracing::warn!(
            dir = %log_name(dir),
            mode = format_args!("{:o}", mode & 0o7777),
            "tightening permissions of a NineSQL directory"
        );
        handle
            .set_permissions(fs::Permissions::from_mode(DIR_MODE))
            .map_err(|e| StorageError::io(dir, e))?;
    }
    Ok(())
}

fn not_a_directory(path: &Path) -> StorageError {
    StorageError::WrongFileType {
        path: path.to_path_buf(),
        expected: "directory (symbolic links are refused)",
    }
}

/// Effective user id of this process.
#[must_use]
pub fn current_uid() -> u32 {
    rustix::process::geteuid().as_raw()
}

/// [`StorageError::NotOwned`] unless `owner` is `current`.
pub(crate) fn check_owner(path: &Path, owner: u32, current: u32) -> Result<(), StorageError> {
    if owner == current {
        Ok(())
    } else {
        Err(StorageError::NotOwned {
            path: path.to_path_buf(),
            owner,
        })
    }
}

/// Removes group / other bits from an existing NineSQL file, if any (a warning is logged).
/// Symbolic links are refused, never followed: NineSQL does not chmod a file it does not own.
///
/// # Errors
/// I/O errors, [`StorageError::WrongFileType`] for a symbolic link or a non-regular file.
pub fn ensure_private_file(file: &Path) -> Result<(), StorageError> {
    let meta = fs::symlink_metadata(file).map_err(|e| StorageError::io(file, e))?;
    if !meta.file_type().is_file() {
        return Err(not_regular(file));
    }
    restrict_mode(file, meta.permissions().mode(), FILE_MODE)
}

fn not_regular(path: &Path) -> StorageError {
    StorageError::WrongFileType {
        path: path.to_path_buf(),
        expected: "regular file (symbolic links, FIFOs and devices are refused)",
    }
}

fn restrict_mode(path: &Path, current: u32, wanted: u32) -> Result<(), StorageError> {
    if current & GROUP_OTHER != 0 {
        tracing::warn!(
            file = %log_name(path),
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
    // The new content is in place once `rename` returns. A failed directory `fsync` only weakens
    // durability across a power loss; reporting it as an error would make callers roll back
    // state (for example stored secrets) that the new file already references.
    if let Err(e) = sync_dir(dir) {
        tracing::warn!(dir = %log_name(dir), io_error = ?e.kind(), "directory fsync failed after an atomic write");
    }
    Ok(())
}

/// Creates `path` with `bytes` atomically, **never replacing** an existing file: a `0600` temp
/// file is written and `fsync`ed next to `path`, then moved with `renameat2(RENAME_NOREPLACE)`;
/// on a file system without it, a hard link then removal of the temp name; without hard links
/// either, a check followed by a rename (a narrow race). The parent directory must exist.
///
/// # Errors
/// [`StorageError::Io`] with [`io::ErrorKind::AlreadyExists`] when `path` exists (a symbolic
/// link included); other I/O errors. On error the temp file is removed and `path` is unchanged.
pub fn write_new_atomic(path: &Path, bytes: &[u8]) -> Result<(), StorageError> {
    let dir = parent_dir(path);
    let (tmp_path, mut tmp) = create_temp(path, dir)?;
    let result = (|| {
        tmp.write_all(bytes)?;
        tmp.sync_all()?;
        drop(tmp);
        move_no_replace(&tmp_path, path)
    })();
    if let Err(e) = result {
        let _ = fs::remove_file(&tmp_path);
        return Err(StorageError::io(path, e));
    }
    if let Err(e) = sync_dir(dir) {
        tracing::warn!(dir = %log_name(dir), io_error = ?e.kind(), "directory fsync failed after an atomic write");
    }
    Ok(())
}

fn move_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    use rustix::fs::{CWD, RenameFlags, renameat_with};
    use rustix::io::Errno;
    match renameat_with(CWD, from, CWD, to, RenameFlags::NOREPLACE) {
        Ok(()) => return Ok(()),
        Err(Errno::INVAL | Errno::NOSYS | Errno::OPNOTSUPP) => {}
        Err(e) => return Err(io::Error::from(e)),
    }
    match fs::hard_link(from, to) {
        Ok(()) => {
            let _ = fs::remove_file(from);
            return Ok(());
        }
        Err(e)
            if e.kind() == io::ErrorKind::Unsupported
                || matches!(e.raw_os_error(), Some(libc::EPERM | libc::EOPNOTSUPP)) => {}
        Err(e) => return Err(e),
    }
    match fs::symlink_metadata(to) {
        Ok(_) => return Err(io::Error::from(io::ErrorKind::AlreadyExists)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    fs::rename(from, to)
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
/// The last path component is never followed if it is a symbolic link, and the file is opened
/// non-blocking, so a FIFO or a device planted in place of a workspace file cannot hang or trick
/// NineSQL.
///
/// # Errors
/// I/O errors, [`StorageError::WrongFileType`] for anything but a regular file,
/// [`StorageError::TooLarge`].
pub fn read_limited(path: &Path, limit: u64) -> Result<Option<Vec<u8>>, StorageError> {
    open_regular(path)?
        .map(|file| read_file(path, file, limit))
        .transpose()
}

/// [`read_limited`], and removes group / other permission bits from the file through the open
/// descriptor (never through a path that could be swapped for a symbolic link).
///
/// # Errors
/// See [`read_limited`].
pub fn read_private(path: &Path, limit: u64) -> Result<Option<Vec<u8>>, StorageError> {
    let Some(file) = open_regular(path)? else {
        return Ok(None);
    };
    let mode = file
        .metadata()
        .map_err(|e| StorageError::io(path, e))?
        .permissions()
        .mode();
    if mode & GROUP_OTHER != 0 {
        tracing::warn!(
            file = %log_name(path),
            mode = format_args!("{:o}", mode & 0o7777),
            "tightening permissions of a NineSQL file"
        );
        file.set_permissions(fs::Permissions::from_mode(FILE_MODE))
            .map_err(|e| StorageError::io(path, e))?;
    }
    read_file(path, file, limit).map(Some)
}

/// Opens a regular file without following a final symbolic link and without blocking.
fn open_regular(path: &Path) -> Result<Option<File>, StorageError> {
    match fs::symlink_metadata(path) {
        Ok(meta) if !meta.file_type().is_file() => return Err(not_regular(path)),
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(StorageError::io(path, e)),
    }
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        // Swapped for a symbolic link between the check and the open.
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => return Err(not_regular(path)),
        Err(e) => return Err(StorageError::io(path, e)),
    };
    let meta = file.metadata().map_err(|e| StorageError::io(path, e))?;
    if !meta.is_file() {
        return Err(not_regular(path));
    }
    Ok(Some(file))
}

fn read_file(path: &Path, file: File, limit: u64) -> Result<Vec<u8>, StorageError> {
    let size = file
        .metadata()
        .map_err(|e| StorageError::io(path, e))?
        .len();
    if size > limit {
        return Err(StorageError::TooLarge {
            path: path.to_path_buf(),
            size,
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
    Ok(buf)
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
    fn directory_symlinks_are_refused_and_not_chmod_ed() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        fs::create_dir(&real).unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o755)).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(matches!(
            ensure_private_dir(&link),
            Err(StorageError::WrongFileType { .. })
        ));
        assert_eq!(mode(&real), 0o755);
        // A dangling link is refused too, and nothing is created at its target.
        let dangling = tmp.path().join("dangling");
        std::os::unix::fs::symlink(tmp.path().join("nowhere"), &dangling).unwrap();
        assert!(ensure_private_dir(&dangling).is_err());
        assert!(!tmp.path().join("nowhere").exists());
        // Symlinked parents are fine.
        ensure_private_dir(&link.join("child")).unwrap();
        assert_eq!(mode(&real.join("child")), DIR_MODE);
    }

    #[test]
    fn foreign_owner_is_refused() {
        assert!(check_owner(Path::new("/x"), 1000, 1000).is_ok());
        let err = check_owner(Path::new("/x"), 0, 1000).unwrap_err();
        assert!(
            matches!(err, StorageError::NotOwned { owner: 0, .. }),
            "{err}"
        );
        // Our own temp directory passes the real check.
        let tmp = tempfile::tempdir().unwrap();
        ensure_private_dir(tmp.path()).unwrap();
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
    fn write_new_never_replaces() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("new.sql");
        write_new_atomic(&path, b"first").unwrap();
        assert_eq!(mode(&path), FILE_MODE);
        let err = write_new_atomic(&path, b"second").unwrap_err();
        assert!(
            matches!(&err, StorageError::Io { source, .. } if source.kind() == io::ErrorKind::AlreadyExists),
            "{err}"
        );
        assert_eq!(fs::read(&path).unwrap(), b"first");
        // A symbolic link (even dangling) in place of the name is not replaced or followed.
        let link = tmp.path().join("link.sql");
        std::os::unix::fs::symlink(tmp.path().join("nowhere"), &link).unwrap();
        assert!(write_new_atomic(&link, b"x").is_err());
        assert!(!tmp.path().join("nowhere").exists());
        // No temp file left behind.
        let count = fs::read_dir(tmp.path()).unwrap().count();
        assert_eq!(count, 2);
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
            Err(StorageError::WrongFileType { .. })
        ));
    }

    #[test]
    fn symlinks_and_fifos_are_refused_without_blocking() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        fs::write(&real, b"x").unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o644)).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(matches!(
            read_private(&link, 10),
            Err(StorageError::WrongFileType { .. })
        ));
        assert!(matches!(
            ensure_private_file(&link),
            Err(StorageError::WrongFileType { .. })
        ));
        // The link target was not chmod-ed through the link.
        assert_eq!(mode(&real), 0o644);

        let fifo = tmp.path().join("fifo");
        let status = std::process::Command::new("mkfifo").arg(&fifo).status();
        if status.is_ok_and(|s| s.success()) {
            assert!(matches!(
                read_limited(&fifo, 10),
                Err(StorageError::WrongFileType { .. })
            ));
        }
    }

    #[test]
    fn read_private_tightens_through_the_descriptor() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("f");
        fs::write(&path, b"abc").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(read_private(&path, 10).unwrap().unwrap(), b"abc");
        assert_eq!(mode(&path), FILE_MODE);
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
