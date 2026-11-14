//! Advisory lock that keeps two NineSQL processes from writing the same workspace at once
//! (ADR-0008 §3, review item L8).
//!
//! The lock is an exclusive `flock(2)` on a lock file (std's [`File::try_lock`]): the kernel
//! releases it when the process exits or crashes, so a stale lock cannot outlive its owner and
//! the lock file itself is never deleted (deleting it would let two processes lock two different
//! inodes).
//!
//! Inside one process, every [`DirLock::acquire`] of the same path shares one lock: `flock`
//! locks belong to an open file description, so a second, independent open in the same process
//! would otherwise be refused.
//!
//! The lock is advisory: it protects against a second NineSQL, not against other programs.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};

use crate::error::StorageError;
use crate::fs::{FILE_MODE, check_owner, current_uid};

/// Locks held by this process, by lock file path.
static HELD: Mutex<BTreeMap<PathBuf, Weak<DirLock>>> = Mutex::new(BTreeMap::new());

/// An exclusive lock held on a lock file until the last clone of the `Arc` is dropped.
#[derive(Debug)]
pub struct DirLock {
    path: PathBuf,
    // Keeps the descriptor, and so the `flock`, alive.
    _file: File,
}

impl DirLock {
    /// Takes the lock on `path` (created `0600` if missing; its directory must exist), or returns
    /// the lock this process already holds on it.
    ///
    /// Never blocks: if another process holds the lock, returns [`StorageError::Locked`].
    ///
    /// # Errors
    /// [`StorageError::Locked`], [`StorageError::WrongFileType`] if `path` is a symbolic link or
    /// not a regular file, [`StorageError::NotOwned`], I/O errors.
    pub fn acquire(path: &Path) -> Result<Arc<Self>, StorageError> {
        // A panic while holding the registry cannot leave it inconsistent: recover it.
        let mut held = HELD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        held.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = held.get(path).and_then(Weak::upgrade) {
            return Ok(lock);
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(FILE_MODE)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)
            .map_err(|e| match e.raw_os_error() {
                Some(libc::ELOOP) => wrong_type(path),
                _ => StorageError::io(path, e),
            })?;
        let meta = file.metadata().map_err(|e| StorageError::io(path, e))?;
        if !meta.is_file() {
            return Err(wrong_type(path));
        }
        check_owner(path, meta.uid(), current_uid())?;
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(StorageError::Locked {
                    path: path.to_path_buf(),
                });
            }
            Err(TryLockError::Error(e)) => return Err(lock_error(path, e)),
        }
        let lock = Arc::new(Self {
            path: path.to_path_buf(),
            _file: file,
        });
        held.insert(path.to_path_buf(), Arc::downgrade(&lock));
        tracing::debug!(path = %path.display(), "workspace lock taken");
        Ok(lock)
    }

    /// The lock file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// [`StorageError::LockUnsupported`] when the file system has no locks (`ENOLCK`,
/// `EOPNOTSUPP`, or std's `Unsupported`), else an I/O error.
fn lock_error(path: &Path, e: io::Error) -> StorageError {
    let unsupported = e.kind() == io::ErrorKind::Unsupported
        || matches!(e.raw_os_error(), Some(code) if code == libc::ENOLCK || code == libc::EOPNOTSUPP);
    if unsupported {
        StorageError::LockUnsupported {
            path: path.to_path_buf(),
        }
    } else {
        StorageError::io(path, e)
    }
}

fn wrong_type(path: &Path) -> StorageError {
    StorageError::WrongFileType {
        path: path.to_path_buf(),
        expected: "regular lock file (symbolic links are refused)",
    }
}

/// Whether another open file description holds a lock on `path` (for tests and diagnostics).
///
/// # Errors
/// I/O errors.
pub fn is_locked_elsewhere(path: &Path) -> Result<bool, StorageError> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(StorageError::io(path, e)),
    };
    match file.try_lock() {
        Ok(()) => Ok(false),
        Err(TryLockError::WouldBlock) => Ok(true),
        Err(TryLockError::Error(e)) => Err(lock_error(path, e)),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn shared_in_process_released_on_drop() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("ninesql.lock");
        let a = DirLock::acquire(&path).unwrap();
        let b = DirLock::acquire(&path).unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            FILE_MODE
        );
        // Another open file description (as another process would have) cannot take it.
        assert!(is_locked_elsewhere(&path).unwrap());
        drop(a);
        assert!(is_locked_elsewhere(&path).unwrap());
        drop(b);
        assert!(!is_locked_elsewhere(&path).unwrap());
        // The lock file stays; taking the lock again works.
        assert!(path.exists());
        let _again = DirLock::acquire(&path).unwrap();
    }

    #[test]
    fn held_by_another_description_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("ninesql.lock");
        let other = File::create(&path).unwrap();
        other.try_lock().unwrap();
        assert!(matches!(
            DirLock::acquire(&path),
            Err(StorageError::Locked { .. })
        ));
        drop(other);
        DirLock::acquire(&path).unwrap();
    }

    #[test]
    fn missing_lock_support_has_its_own_error() {
        let p = Path::new("/x/ninesql.lock");
        for e in [
            io::Error::from_raw_os_error(libc::ENOLCK),
            io::Error::from_raw_os_error(libc::EOPNOTSUPP),
            io::Error::from(io::ErrorKind::Unsupported),
        ] {
            let err = lock_error(p, e);
            assert!(matches!(err, StorageError::LockUnsupported { .. }), "{err}");
            assert!(err.to_string().contains("file locking is not available"));
        }
        assert!(matches!(
            lock_error(p, io::Error::from_raw_os_error(libc::EIO)),
            StorageError::Io { .. }
        ));
    }

    #[test]
    fn symlinked_lock_file_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("target");
        let path = tmp.path().join("ninesql.lock");
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(matches!(
            DirLock::acquire(&path),
            Err(StorageError::WrongFileType { .. })
        ));
        assert!(!target.exists());
    }
}
