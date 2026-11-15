//! Errors of the DBeaver importer. They name paths and positions, never file content: DBeaver
//! files hold credentials (I1), and `serde_json` messages can quote the offending value.

use std::io;
use std::path::PathBuf;

/// Why a DBeaver file or folder could not be used.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DbeaverError {
    /// The file or directory could not be read.
    #[error("cannot read {path}: {kind}")]
    Io {
        /// Path concerned.
        path: PathBuf,
        /// I/O error kind (never the OS message, which is kind-only anyway).
        kind: io::ErrorKind,
    },
    /// The path is not a regular file (directory, FIFO, device…): it is not read.
    #[error("{path} is not a regular file")]
    NotAFile {
        /// Path concerned.
        path: PathBuf,
    },
    /// The file is larger than the importer accepts.
    #[error("{path} is larger than {limit} bytes and was not read")]
    TooLarge {
        /// Path concerned.
        path: PathBuf,
        /// Size limit in bytes.
        limit: u64,
    },
    /// The file is not valid JSON. The position is given; the content never is.
    #[error("{path} is not valid JSON (line {line}, column {column})")]
    InvalidJson {
        /// Path concerned.
        path: PathBuf,
        /// 1-based line of the error.
        line: usize,
        /// 1-based column of the error.
        column: usize,
    },
    /// The file is valid JSON but its top level is not an object.
    #[error("{path} does not contain a JSON object")]
    NotAnObject {
        /// Path concerned.
        path: PathBuf,
    },
    /// The read was stopped through a [`crate::CancelFlag`]; nothing of it should be used.
    #[error("the DBeaver import was cancelled")]
    Cancelled,
    /// The folder is neither a DBeaver workspace nor a DBeaver project.
    #[error("{path} is not a DBeaver workspace or project folder")]
    NotDbeaver {
        /// Path concerned.
        path: PathBuf,
    },
}

impl DbeaverError {
    pub(crate) fn io(path: impl Into<PathBuf>, err: &io::Error) -> Self {
        Self::Io {
            path: path.into(),
            kind: err.kind(),
        }
    }
}
