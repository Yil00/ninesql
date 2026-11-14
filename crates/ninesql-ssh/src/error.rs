//! Typed, user-presentable errors. Messages never contain a secret: passwords and passphrases
//! are not in scope of any variant, and server-provided text (banners, disconnect reasons) is
//! not echoed.

use std::path::PathBuf;

use crate::fingerprint::Fingerprint;
use crate::known_hosts::KnownHostsSource;

/// An unknown host key that needs the user's confirmation (ADR-0011 §6). Never auto-accepted
/// under [`crate::HostKeyPolicy::Strict`].
///
/// The UI shows [`UnknownHostKey::fingerprint`] (and the key type); if the user confirms, the
/// core calls [`crate::KnownHostsStore::trust`] and opens the tunnel again. The retried
/// connection checks the presented key against the file, so a different key at retry time is
/// still caught.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnknownHostKey {
    /// Index of the hop in the chain (jump hosts first, the tunnel server last).
    pub hop: usize,
    /// SSH host as configured.
    pub host: String,
    /// SSH port.
    pub port: u16,
    /// Key type (`ssh-ed25519`, `ecdsa-sha2-nistp256`…).
    pub key_type: String,
    /// SHA-256 fingerprint, `SHA256:<base64>` when displayed.
    pub fingerprint: Fingerprint,
    /// Public key blob (wire encoding), written to `known_hosts` if the user confirms.
    pub key_blob: Vec<u8>,
    /// Key types known for this host that NineSQL cannot negotiate (for example `ssh-rsa`):
    /// when not empty the UI **must** say that this host is known under another key type
    /// (also under the accept-new policy, which then asks instead of accepting).
    pub other_key_types: Vec<String>,
}

/// Error of `ninesql-ssh`.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SshError {
    /// The host key is not on file: the user must confirm the fingerprint first.
    #[error(
        "the SSH host key of {}:{} is not known; confirm its fingerprint {} ({}) to continue",
        .0.host, .0.port, .0.fingerprint, .0.key_type
    )]
    HostKeyNeedsConfirmation(Box<UnknownHostKey>),
    /// The host presented a different key than the one on file. Hard error: the old key must be
    /// removed explicitly before a new one can be trusted (ADR-0011 §6).
    #[error(
        "WARNING: the SSH host key of {host}:{port} has changed (on file: {known_fingerprint}, presented: {presented_fingerprint}). Someone may be intercepting the connection. Remove the old key from line {line} of {file:?} known_hosts only if you know why it changed"
    )]
    HostKeyChanged {
        /// Hop index, when raised while opening a tunnel.
        hop: Option<usize>,
        /// SSH host.
        host: String,
        /// SSH port.
        port: u16,
        /// File holding the old key.
        file: KnownHostsSource,
        /// Line of the old key.
        line: usize,
        /// Fingerprint on file.
        known_fingerprint: Fingerprint,
        /// Fingerprint presented by the server.
        presented_fingerprint: Fingerprint,
    },
    /// The presented key is marked `@revoked`.
    #[error("the SSH host key of {host}:{port} is revoked (line {line} of {file:?} known_hosts)")]
    HostKeyRevoked {
        /// Hop index, when raised while opening a tunnel.
        hop: Option<usize>,
        /// SSH host.
        host: String,
        /// SSH port.
        port: u16,
        /// File holding the `@revoked` line.
        file: KnownHostsSource,
        /// Line of the `@revoked` entry.
        line: usize,
    },
    /// The server presented a host certificate or a key type NineSQL does not verify.
    #[error("the SSH server {host}:{port} presented an unsupported host key ({detail})")]
    UnsupportedHostKey {
        /// SSH host.
        host: String,
        /// SSH port.
        port: u16,
        /// What was presented.
        detail: String,
    },
    /// A host key blob that is not a valid SSH public key.
    #[error("invalid SSH host key")]
    InvalidHostKey,
    /// A host name that cannot be written to `known_hosts`.
    #[error("invalid SSH host name")]
    InvalidHost,
    /// Reading or writing a `known_hosts` file failed.
    #[error("known_hosts: {0}")]
    KnownHosts(String),
    /// The tunnel description is not valid.
    #[error("invalid SSH tunnel: {0}")]
    InvalidSpec(&'static str),
    /// TCP connection to the first hop, or `direct-tcpip` channel to the next hop, failed.
    #[error("cannot reach SSH hop {hop} ({host}:{port}): {detail}")]
    Connect {
        /// Hop index.
        hop: usize,
        /// SSH host.
        host: String,
        /// SSH port.
        port: u16,
        /// Cause.
        detail: String,
    },
    /// The hop did not complete the connection, handshake or authentication in time.
    #[error("SSH hop {hop} ({host}:{port}) timed out")]
    Timeout {
        /// Hop index.
        hop: usize,
        /// SSH host.
        host: String,
        /// SSH port.
        port: u16,
    },
    /// SSH protocol failure (no common algorithm, malformed packet…).
    #[error("SSH error on hop {hop} ({host}:{port}): {detail}")]
    Protocol {
        /// Hop index.
        hop: usize,
        /// SSH host.
        host: String,
        /// SSH port.
        port: u16,
        /// Cause (from the SSH library; never contains credentials).
        detail: String,
    },
    /// The server refused the credentials.
    #[error("SSH authentication failed for {user}@{host}:{port} ({method})")]
    AuthFailed {
        /// Hop index.
        hop: usize,
        /// SSH host.
        host: String,
        /// SSH port.
        port: u16,
        /// User name.
        user: String,
        /// Method tried (`password`, `public key`, `agent`).
        method: &'static str,
    },
    /// The server accepted the credentials but asks for a further factor, which NineSQL does
    /// not support yet.
    #[error(
        "SSH server {host}:{port} requires an additional authentication step (multi-factor), which is not supported"
    )]
    PartialAuth {
        /// Hop index.
        hop: usize,
        /// SSH host.
        host: String,
        /// SSH port.
        port: u16,
    },
    /// The private key file cannot be used.
    #[error("cannot use SSH key {path}: {reason}")]
    KeyFile {
        /// Hop index.
        hop: usize,
        /// Key path.
        path: PathBuf,
        /// Why.
        reason: KeyFileError,
    },
    /// The SSH agent is not available or failed.
    #[error("SSH agent: {0}")]
    Agent(String),
    /// The local listener could not be created.
    #[error("cannot open the local tunnel port: {0}")]
    Listener(String),
    /// A blocking task failed (runtime shutting down).
    #[error("internal error: {0}")]
    Internal(&'static str),
}

/// Why a private key file was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum KeyFileError {
    /// The file cannot be read.
    #[error("cannot read the file ({0})")]
    Unreadable(String),
    /// Group or others can read the file (OpenSSH refuses such keys too).
    #[error("permissions {0:o} are too open; run `chmod 600` on the key")]
    PermissionsTooOpen(u32),
    /// The file belongs to another user (only the current user or root are accepted, like
    /// OpenSSH).
    #[error("the file belongs to another user")]
    WrongOwner,
    /// Not a regular file, or larger than the limit.
    #[error("not a regular key file of reasonable size")]
    NotAKeyFile,
    /// The key is encrypted and no passphrase was given.
    #[error("the key is encrypted and needs a passphrase")]
    PassphraseRequired,
    /// The key cannot be parsed or decrypted (wrong passphrase or unsupported format).
    #[error("cannot decode the key (wrong passphrase or unsupported format)")]
    Undecodable,
    /// Key algorithm not supported (RSA keys are not supported yet, see the crate docs).
    #[error("unsupported key type {0}")]
    UnsupportedType(String),
}

impl SshError {
    /// The unknown host key awaiting confirmation, if this is that error.
    #[must_use]
    pub fn unknown_host_key(&self) -> Option<&UnknownHostKey> {
        match self {
            Self::HostKeyNeedsConfirmation(k) => Some(k),
            _ => None,
        }
    }
}
