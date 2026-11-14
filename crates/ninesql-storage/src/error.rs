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
    pub(crate) fn io(path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}
