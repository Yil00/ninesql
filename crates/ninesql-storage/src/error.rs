//! Storage errors.

use std::io;
use std::path::PathBuf;

/// Error of a storage operation. Messages are user-presentable.
///
/// Parse details never echo string values from the file (see [`crate::document`]).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StorageError {
    /// `$HOME` is missing or relative and an XDG variable needs it as a fallback.
    #[error("cannot locate the user directories: $HOME is not set to an absolute path")]
    NoHome,
    /// A project name that cannot be used as a directory name.
    #[error(
        "invalid project name: use 1 to 128 characters without slashes, backslashes or control characters, not starting with a dot, not ending with a space or a dot"
    )]
    InvalidProjectName,
    /// An I/O failure on a path.
    #[error("I/O error on {path}: {source}")]
    Io {
        /// Path concerned.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: io::Error,
    },
    /// A path exists but is not what NineSQL expects (for example a file where a directory goes).
    #[error("{path} is not a {expected}")]
    WrongFileType {
        /// Path concerned.
        path: PathBuf,
        /// What was expected (`directory`, `regular file`).
        expected: &'static str,
    },
    /// A NineSQL directory or file belongs to another user. NineSQL refuses to use it: whoever owns
    /// it could read or replace workspace files.
    #[error(
        "{path} belongs to another user (uid {owner}); NineSQL only uses directories and files owned by the current user"
    )]
    NotOwned {
        /// Path concerned.
        path: PathBuf,
        /// Owner found.
        owner: u32,
    },
    /// Another NineSQL process holds the workspace lock.
    #[error(
        "another NineSQL process is using this workspace ({path} is locked); close it before starting a new one"
    )]
    Locked {
        /// Lock file.
        path: PathBuf,
    },
    /// The file system of the workspace does not support file locks (some network or FUSE file
    /// systems), so NineSQL cannot make sure it is the only process writing the workspace.
    #[error(
        "file locking is not available on the file system holding {path}; NineSQL cannot protect the workspace against a second process. Move the XDG data directory to a local file system"
    )]
    LockUnsupported {
        /// Lock file.
        path: PathBuf,
    },
    /// A file is larger than NineSQL accepts for its kind.
    #[error("{path} is too large ({size} bytes, the limit is {limit} bytes)")]
    TooLarge {
        /// Path concerned.
        path: PathBuf,
        /// Actual size.
        size: u64,
        /// Accepted maximum.
        limit: u64,
    },
    /// The file is not valid JSON / TOML, or does not match the expected structure.
    #[error("{path} is not a valid {document} file: {detail}")]
    Parse {
        /// Path concerned.
        path: PathBuf,
        /// Document name (`connections`, `settings`…).
        document: &'static str,
        /// Parser message with string values redacted.
        detail: String,
    },
    /// The top-level `schema_version` is missing or not a positive integer.
    #[error("{path} has no valid top-level `schema_version`")]
    MissingSchemaVersion {
        /// Path concerned.
        path: PathBuf,
    },
    /// The file was written by a newer NineSQL. It is left untouched.
    #[error(
        "{path} uses schema version {found}; this NineSQL reads up to version {supported}. Update NineSQL to open it"
    )]
    NewerVersion {
        /// Path concerned.
        path: PathBuf,
        /// Version found in the file.
        found: u32,
        /// Newest version this build understands.
        supported: u32,
    },
    /// No migration step is registered from a version (a bug: every bump ships a migration).
    #[error("no migration of the {document} file from schema version {from}")]
    MissingMigration {
        /// Document name.
        document: &'static str,
        /// Version without a migration step.
        from: u32,
    },
    /// A migration step failed. The original file is left untouched.
    #[error("migrating {path} from schema version {from} failed: {detail}")]
    Migration {
        /// Path concerned.
        path: PathBuf,
        /// Version the failing step starts from.
        from: u32,
        /// Step message.
        detail: String,
    },
    /// The query history database (`history.sqlite`) failed: SQLite error, damaged file. The
    /// detail is SQLite's message; it never contains statement text or values.
    #[error("history database {path}: {detail}")]
    History {
        /// Path concerned.
        path: PathBuf,
        /// SQLite message.
        detail: String,
    },
    /// The in-memory value could not be serialized.
    #[error("cannot serialize the {document} file: {detail}")]
    Serialize {
        /// Document name.
        document: &'static str,
        /// Serializer message.
        detail: String,
    },
}

impl StorageError {
    /// A short, stable name of the error kind, safe to log at any level: no path, no parser
    /// or SQLite message (ADR-0029 §3.2). The message itself is for the UI.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::NoHome => "storage-no-home",
            Self::InvalidProjectName => "storage-invalid-project-name",
            Self::Io { .. } => "storage-io",
            Self::WrongFileType { .. } => "storage-wrong-file-type",
            Self::NotOwned { .. } => "storage-not-owned",
            Self::Locked { .. } => "storage-locked",
            Self::LockUnsupported { .. } => "storage-lock-unsupported",
            Self::TooLarge { .. } => "storage-too-large",
            Self::Parse { .. } => "storage-parse",
            Self::MissingSchemaVersion { .. } => "storage-missing-schema-version",
            Self::NewerVersion { .. } => "storage-newer-version",
            Self::MissingMigration { .. } => "storage-missing-migration",
            Self::Migration { .. } => "storage-migration",
            Self::History { .. } => "storage-history",
            Self::Serialize { .. } => "storage-serialize",
        }
    }

    /// The I/O error kind of an [`StorageError::Io`], safe to log (no path, no OS message).
    #[must_use]
    pub fn io_kind(&self) -> Option<io::ErrorKind> {
        match self {
            Self::Io { source, .. } => Some(source.kind()),
            _ => None,
        }
    }

    pub(crate) fn io(path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}
