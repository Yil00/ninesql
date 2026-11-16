//! The export file: written only to a new file the user chose (I4), never through a symbolic
//! link, private while it is written, and never left half-written.
//!
//! [`OutputFile::create`]:
//! 1. refuses a relative path, a path without a file name, a missing parent directory, a target
//!    that is a symbolic link (even a dangling one) or a non-regular file, and an existing file
//!    unless [`Overwrite::Confirmed`];
//! 2. creates a temporary file **in the same directory** with `O_CREAT | O_EXCL | O_NOFOLLOW |
//!    O_CLOEXEC` and mode `0600` (named `.<name>.<random>.ninesql-part`);
//! 3. [`OutputFile::commit`] flushes, `fsync`s and moves it into place, then `fsync`s the
//!    directory. Without an overwrite confirmation the move is a hard link, which fails if the
//!    target appeared meanwhile (no race replaces a file the user did not confirm); on a file
//!    system without hard links it falls back to a check followed by `rename` (a narrow race,
//!    documented). With a confirmation it is a `rename`, which replaces a symbolic link itself,
//!    never the file it points to;
//! 4. on error, cancel or drop before `commit`, the temporary file is removed.
//!
//! The final file keeps mode `0600`: an export may hold sensitive data, so it is private to the
//! user like every other NineSQL file. Path policy (DBeaver workspaces, NineSQL's own storage) is
//! the caller's: `ninesql-core` checks the canonical path before calling `create`.

use std::fs::{self, File, OpenOptions};
use std::hash::{BuildHasher, Hasher};
use std::io::{self, BufWriter, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::TransferError;

/// Mode of the export file.
pub const EXPORT_FILE_MODE: u32 = 0o600;

/// Size of the write buffer in front of the file.
pub const WRITE_BUFFER_BYTES: usize = 64 * 1024;

/// Whether an existing file may be replaced.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum Overwrite {
    /// Fail with [`TransferError::AlreadyExists`] when the file exists (default).
    #[default]
    Refuse,
    /// The user explicitly confirmed replacing an existing regular file.
    Confirmed,
}

/// A file being written. Dropping it before [`OutputFile::commit`] removes the temporary file.
#[derive(Debug)]
pub struct OutputFile {
    target: PathBuf,
    temp: PathBuf,
    writer: Option<BufWriter<File>>,
    overwrite: Overwrite,
    written: u64,
    committed: bool,
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn random_suffix() -> u64 {
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    h.write_u32(std::process::id());
    if let Ok(d) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        h.write_u128(d.as_nanos());
    }
    h.finish()
}

/// Checks the target path: absolute, a file name, an existing parent directory, not a symbolic
/// link, not a non-regular file, and not an existing file unless `overwrite` is confirmed.
///
/// # Errors
/// [`TransferError::InvalidPath`], [`TransferError::NotARegularFile`],
/// [`TransferError::AlreadyExists`].
pub fn check_target(target: &Path, overwrite: Overwrite) -> Result<(), TransferError> {
    if !target.is_absolute() {
        return Err(TransferError::InvalidPath(
            "the path is not absolute".into(),
        ));
    }
    if target.file_name().is_none() {
        return Err(TransferError::InvalidPath(
            "the path has no file name".into(),
        ));
    }
    let parent = target
        .parent()
        .ok_or_else(|| TransferError::InvalidPath("the path has no directory".into()))?;
    match fs::metadata(parent) {
        Ok(m) if m.is_dir() => {}
        Ok(_) => {
            return Err(TransferError::InvalidPath(
                "the parent is not a directory".into(),
            ));
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(TransferError::InvalidPath(
                "the directory does not exist".into(),
            ));
        }
        Err(e) => return Err(e.into()),
    }
    match fs::symlink_metadata(target) {
        Ok(m) if m.file_type().is_symlink() || !m.is_file() => {
            Err(TransferError::NotARegularFile(target.to_path_buf()))
        }
        Ok(_) if overwrite == Overwrite::Refuse => {
            Err(TransferError::AlreadyExists(target.to_path_buf()))
        }
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

impl OutputFile {
    /// Creates the temporary file next to `target` (see the [module documentation](self)).
    ///
    /// # Errors
    /// See [`check_target`]; I/O errors creating the temporary file.
    pub fn create(target: &Path, overwrite: Overwrite) -> Result<Self, TransferError> {
        check_target(target, overwrite)?;
        let parent = target.parent().unwrap_or_else(|| Path::new("/"));
        let name = target.file_name().unwrap_or_default().to_string_lossy();
        // Keep the temporary name short whatever the target name.
        let short: String = name.chars().take(64).collect();
        let mut last_err = None;
        for _ in 0..16 {
            let temp = parent.join(format!(".{short}.{:016x}.ninesql-part", random_suffix()));
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(EXPORT_FILE_MODE)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&temp)
            {
                Ok(file) => {
                    return Ok(Self {
                        target: target.to_path_buf(),
                        temp,
                        writer: Some(BufWriter::with_capacity(WRITE_BUFFER_BYTES, file)),
                        overwrite,
                        written: 0,
                        committed: false,
                    });
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => last_err = Some(e),
                Err(e) => return Err(e.into()),
            }
        }
        Err(last_err
            .unwrap_or_else(|| io::Error::other("no temporary name available"))
            .into())
    }

    /// The final path.
    #[must_use]
    pub fn target(&self) -> &Path {
        &self.target
    }

    /// The temporary path (tests and diagnostics).
    #[must_use]
    pub fn temp_path(&self) -> &Path {
        &self.temp
    }

    /// Bytes written so far.
    #[must_use]
    pub fn written(&self) -> u64 {
        self.written
    }

    /// Moves the file into place (see the [module documentation](self)). Returns the bytes
    /// written.
    ///
    /// # Errors
    /// I/O errors; [`TransferError::AlreadyExists`] when the target appeared meanwhile and
    /// overwriting was not confirmed. The temporary file is removed on error.
    pub fn commit(mut self) -> Result<u64, TransferError> {
        let result = self.commit_inner();
        if result.is_ok() {
            self.committed = true;
        }
        result.map(|()| self.written)
    }

    fn commit_inner(&mut self) -> Result<(), TransferError> {
        let writer = self
            .writer
            .take()
            .ok_or_else(|| io::Error::other("export file already closed"))?;
        let file = writer.into_inner().map_err(|e| e.into_error())?;
        file.sync_all()?;
        drop(file);
        match self.overwrite {
            Overwrite::Refuse => match fs::hard_link(&self.temp, &self.target) {
                Ok(()) => {
                    let _ = fs::remove_file(&self.temp);
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    return Err(TransferError::AlreadyExists(self.target.clone()));
                }
                Err(_) => {
                    // No hard links on this file system (vfat, some FUSE mounts): check, then
                    // rename. A file created in between would be replaced (narrow race).
                    match fs::symlink_metadata(&self.target) {
                        Ok(_) => return Err(TransferError::AlreadyExists(self.target.clone())),
                        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                        Err(e) => return Err(e.into()),
                    }
                    fs::rename(&self.temp, &self.target)?;
                }
            },
            Overwrite::Confirmed => {
                if let Ok(m) = fs::symlink_metadata(&self.target)
                    && m.is_dir()
                {
                    return Err(TransferError::NotARegularFile(self.target.clone()));
                }
                fs::rename(&self.temp, &self.target)?;
            }
        }
        if let Some(parent) = self.target.parent()
            && let Ok(dir) = File::open(parent)
        {
            let _ = dir.sync_all();
        }
        Ok(())
    }

    /// Removes the temporary file (cancel or error).
    pub fn abort(mut self) {
        self.remove_temp();
    }

    fn remove_temp(&mut self) {
        self.writer.take();
        if !self.committed {
            match fs::remove_file(&self.temp) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => tracing::warn!(error = %e, "could not remove a partial export file"),
            }
        }
    }
}

impl Write for OutputFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let w = self
            .writer
            .as_mut()
            .ok_or_else(|| io::Error::other("export file already closed"))?;
        let n = w.write(buf)?;
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.writer.as_mut() {
            Some(w) => w.flush(),
            None => Ok(()),
        }
    }
}

impl Drop for OutputFile {
    fn drop(&mut self) {
        self.remove_temp();
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn writes_privately_and_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("out.csv");
        let mut f = OutputFile::create(&target, Overwrite::Refuse).unwrap();
        let temp = f.temp_path().to_path_buf();
        assert_eq!(
            fs::metadata(&temp).unwrap().permissions().mode() & 0o777,
            0o600
        );
        f.write_all(b"a,b\n").unwrap();
        assert!(!target.exists());
        assert_eq!(f.commit().unwrap(), 4);
        assert!(!temp.exists());
        assert_eq!(fs::read(&target).unwrap(), b"a,b\n");
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o600
        );
        // Existing file: refused unless confirmed.
        assert!(matches!(
            OutputFile::create(&target, Overwrite::Refuse),
            Err(TransferError::AlreadyExists(_))
        ));
        let mut f = OutputFile::create(&target, Overwrite::Confirmed).unwrap();
        f.write_all(b"x").unwrap();
        f.commit().unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"x");
    }

    #[test]
    fn target_appearing_meanwhile_is_not_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("race.csv");
        let mut f = OutputFile::create(&target, Overwrite::Refuse).unwrap();
        f.write_all(b"new").unwrap();
        fs::write(&target, b"theirs").unwrap();
        let temp = f.temp_path().to_path_buf();
        assert!(matches!(f.commit(), Err(TransferError::AlreadyExists(_))));
        assert_eq!(fs::read(&target).unwrap(), b"theirs");
        assert!(!temp.exists());
    }

    #[test]
    fn symlinks_are_refused_even_with_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim");
        fs::write(&victim, b"keep").unwrap();
        let link = dir.path().join("link.csv");
        std::os::unix::fs::symlink(&victim, &link).unwrap();
        for ow in [Overwrite::Refuse, Overwrite::Confirmed] {
            assert!(matches!(
                OutputFile::create(&link, ow),
                Err(TransferError::NotARegularFile(_))
            ));
        }
        let dangling = dir.path().join("dangling.csv");
        std::os::unix::fs::symlink(dir.path().join("nowhere"), &dangling).unwrap();
        assert!(matches!(
            OutputFile::create(&dangling, Overwrite::Refuse),
            Err(TransferError::NotARegularFile(_))
        ));
        assert_eq!(fs::read(&victim).unwrap(), b"keep");
        assert!(!dir.path().join("nowhere").exists());
    }

    #[test]
    fn abort_and_drop_remove_the_partial_file() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("partial.csv");
        let mut f = OutputFile::create(&target, Overwrite::Refuse).unwrap();
        f.write_all(b"half").unwrap();
        let temp = f.temp_path().to_path_buf();
        f.abort();
        assert!(!temp.exists() && !target.exists());
        let f = OutputFile::create(&target, Overwrite::Refuse).unwrap();
        let temp = f.temp_path().to_path_buf();
        drop(f);
        assert!(!temp.exists() && !target.exists());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn bad_paths() {
        assert!(matches!(
            check_target(Path::new("rel.csv"), Overwrite::Refuse),
            Err(TransferError::InvalidPath(_))
        ));
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            check_target(&dir.path().join("missing/x.csv"), Overwrite::Refuse),
            Err(TransferError::InvalidPath(_))
        ));
        assert!(matches!(
            check_target(dir.path(), Overwrite::Confirmed),
            Err(TransferError::NotARegularFile(_))
        ));
    }
}
