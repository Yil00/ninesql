//! Errors of the transfer crate. Messages never quote a value from the data or the file (I1:
//! a CSV cell or a result value may be a secret), only positions, sizes and reasons.

use std::io;
use std::path::PathBuf;

/// A transfer error, user-presentable.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TransferError {
    /// An option is out of range or inconsistent.
    #[error("invalid option: {0}")]
    InvalidOption(String),
    /// The target path is not absolute, has no file name or its directory does not exist.
    #[error("invalid export path: {0}")]
    InvalidPath(String),
    /// The target file exists and overwriting it was not confirmed.
    #[error("the file already exists: {}", .0.display())]
    AlreadyExists(PathBuf),
    /// The target is a symbolic link, a directory or another non-regular file.
    #[error("refusing to write through a symbolic link or over a non-regular file: {}", .0.display())]
    NotARegularFile(PathBuf),
    /// The target lies in a place NineSQL never writes to (a DBeaver workspace, I4, or
    /// NineSQL's own storage holding secret stores, I1).
    #[error("refusing to write there: {reason}")]
    ForbiddenLocation {
        /// What the place is.
        reason: String,
    },
    /// The file is larger than the import limit.
    #[error("the file is larger than the limit of {limit} bytes")]
    FileTooLarge {
        /// The limit.
        limit: u64,
    },
    /// The result holds something the format cannot write (documents in a tabular format).
    #[error("unsupported result for this format: {0}")]
    Unsupported(String),
    /// A fatal CSV structure error (unterminated quote, record over the size cap).
    #[error("CSV error at line {line}: {reason}")]
    Csv {
        /// 1-based line where the record starts.
        line: u64,
        /// What is wrong (never the content).
        reason: String,
    },
    /// The transfer was cancelled.
    #[error("cancelled")]
    Cancelled,
    /// Reading or writing a file failed.
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
}
