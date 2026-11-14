//! SSH authentication of one hop: password, private key (OpenSSH format, optional passphrase),
//! or `ssh-agent` (ADR-0011 §5).
//!
//! Secrets (I1) arrive as [`SecretString`] (zeroized on drop, redacted `Debug`) and are moved
//! into the authentication step, which drops them as soon as it returns. The key file is read
//! into a zeroized buffer sized up front (no reallocation leaves copies behind); the decoded
//! private key zeroizes itself on drop (`ssh-key`). `russh` 0.65.1 copies a password into its
//! own request (`auth::Method::Password`), which it zeroizes on drop.

use std::fmt;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use russh::client::{AuthResult, Handle, Handler};
use russh::keys::agent::AgentIdentity;
use russh::keys::agent::client::AgentClient;
use russh::keys::{Algorithm, HashAlg, PrivateKey, PrivateKeyWithHashAlg};
use secrecy::{ExposeSecret, SecretString};
use zeroize::Zeroizing;

use crate::error::{KeyFileError, SshError};

/// Largest private key file read (1 MiB).
const MAX_KEY_FILE_SIZE: u64 = 1024 * 1024;
/// Agent identities tried at most per hop (servers usually cut at `MaxAuthTries` = 6).
const MAX_AGENT_IDENTITIES: usize = 5;

/// How one hop authenticates. `Debug` never shows a secret.
#[non_exhaustive]
pub enum HopAuth {
    /// Keys held by `ssh-agent` (`SSH_AUTH_SOCK`).
    Agent,
    /// Password.
    Password(SecretString),
    /// Private key file in OpenSSH (or PuTTY) format; Ed25519 and ECDSA keys.
    PublicKey {
        /// Key path.
        key_path: PathBuf,
        /// Passphrase for an encrypted key.
        passphrase: Option<SecretString>,
    },
}

impl HopAuth {
    /// Name of the method, for messages.
    #[must_use]
    pub fn method(&self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Password(_) => "password",
            Self::PublicKey { .. } => "public key",
        }
    }
}

impl fmt::Debug for HopAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Agent => f.write_str("Agent"),
            Self::Password(_) => f.write_str("Password(<redacted>)"),
            Self::PublicKey {
                key_path,
                passphrase,
            } => f
                .debug_struct("PublicKey")
                .field("key_path", key_path)
                .field("passphrase", &passphrase.as_ref().map(|_| "<redacted>"))
                .finish(),
        }
    }
}

/// Context of the hop being authenticated, for error messages.
pub(crate) struct HopRef<'a> {
    pub index: usize,
    pub host: &'a str,
    pub port: u16,
    pub user: &'a str,
}

impl HopRef<'_> {
    fn failed(&self, method: &'static str) -> SshError {
        SshError::AuthFailed {
            hop: self.index,
            host: self.host.to_owned(),
            port: self.port,
            user: self.user.to_owned(),
            method,
        }
    }

    fn protocol(&self, e: &russh::Error) -> SshError {
        SshError::Protocol {
            hop: self.index,
            host: self.host.to_owned(),
            port: self.port,
            detail: e.to_string(),
        }
    }

    fn result(&self, r: AuthResult, method: &'static str) -> Result<(), SshError> {
        match r {
            AuthResult::Success => Ok(()),
            AuthResult::Failure {
                partial_success: true,
                ..
            } => Err(SshError::PartialAuth {
                hop: self.index,
                host: self.host.to_owned(),
                port: self.port,
            }),
            AuthResult::Failure { .. } => Err(self.failed(method)),
        }
    }
}

/// Authenticates `handle` as `hop.user`. Consumes `auth`: the secret is dropped (zeroized) when
/// this returns.
pub(crate) async fn authenticate<H: Handler>(
    handle: &mut Handle<H>,
    hop: &HopRef<'_>,
    auth: HopAuth,
) -> Result<(), SshError> {
    match auth {
        HopAuth::Password(password) => {
            let r = handle
                .authenticate_password(hop.user, password.expose_secret())
                .await
                .map_err(|e| hop.protocol(&e))?;
            drop(password);
            hop.result(r, "password")
        }
        HopAuth::PublicKey {
            key_path,
            passphrase,
        } => {
            let index = hop.index;
            let path = key_path.clone();
            let key = tokio::task::spawn_blocking(move || load_key(index, &path, passphrase))
                .await
                .map_err(|_| SshError::Internal("key loading task failed"))??;
            let key = PrivateKeyWithHashAlg::new(Arc::new(key), None);
            let r = handle
                .authenticate_publickey(hop.user, key)
                .await
                .map_err(|e| hop.protocol(&e))?;
            hop.result(r, "public key")
        }
        HopAuth::Agent => authenticate_agent(handle, hop).await,
    }
}

/// Reads and decodes a private key file (blocking).
fn load_key(
    hop: usize,
    path: &Path,
    passphrase: Option<SecretString>,
) -> Result<PrivateKey, SshError> {
    let err = |reason| SshError::KeyFile {
        hop,
        path: path.to_path_buf(),
        reason,
    };
    let unreadable = |e: std::io::Error| err(KeyFileError::Unreadable(e.kind().to_string()));
    // Open first, then check the open descriptor (no gap between the check and the read).
    // Non-blocking open: a FIFO planted at the path cannot hang the blocking pool. Symbolic
    // links are followed, like OpenSSH.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        .map_err(unreadable)?;
    let meta = file.metadata().map_err(unreadable)?;
    if !meta.is_file() || meta.len() > MAX_KEY_FILE_SIZE {
        return Err(err(KeyFileError::NotAKeyFile));
    }
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(err(KeyFileError::PermissionsTooOpen(mode)));
    }
    match current_uid() {
        Some(uid) if meta.uid() != uid && meta.uid() != 0 => {
            return Err(err(KeyFileError::WrongOwner));
        }
        Some(_) => {}
        None => tracing::warn!("cannot determine the current user; key file owner not checked"),
    }
    let len = usize::try_from(meta.len()).map_err(|_| err(KeyFileError::NotAKeyFile))?;
    // Capacity for the whole file plus one byte, and reads capped at the size seen by fstat:
    // the buffer never reallocates, so no unzeroized copy of the key is left on the heap.
    let mut bytes = Zeroizing::new(Vec::with_capacity(len + 1));
    file.take(meta.len())
        .read_to_end(&mut bytes)
        .map_err(unreadable)?;
    let text = std::str::from_utf8(&bytes).map_err(|_| err(KeyFileError::Undecodable))?;
    let key = russh::keys::decode_secret_key(text, passphrase.as_ref().map(|p| p.expose_secret()))
        .map_err(|e| match e {
            russh::keys::Error::KeyIsEncrypted => err(KeyFileError::PassphraseRequired),
            _ => err(KeyFileError::Undecodable),
        })?;
    drop(passphrase);
    // ssh-key decodes some encrypted keys lazily: refuse a key that is still encrypted.
    if key.is_encrypted() {
        return Err(err(KeyFileError::PassphraseRequired));
    }
    match key.algorithm() {
        Algorithm::Ed25519 | Algorithm::Ecdsa { .. } => Ok(key),
        other => Err(err(KeyFileError::UnsupportedType(
            other.as_str().to_owned(),
        ))),
    }
}

/// Effective uid of this process, from the owner of `/proc/self` (no `unsafe` libc call).
fn current_uid() -> Option<u32> {
    std::fs::metadata("/proc/self").ok().map(|m| m.uid())
}

async fn authenticate_agent<H: Handler>(
    handle: &mut Handle<H>,
    hop: &HopRef<'_>,
) -> Result<(), SshError> {
    let mut agent = AgentClient::connect_env()
        .await
        .map_err(|_| SshError::Agent("no SSH agent is reachable (is SSH_AUTH_SOCK set?)".into()))?;
    let identities = agent
        .request_identities()
        .await
        .map_err(|_| SshError::Agent("the agent did not list its keys".into()))?;
    let keys: Vec<_> = identities
        .into_iter()
        .filter_map(|id| match id {
            AgentIdentity::PublicKey { key, .. } => Some(key),
            // Host certificates for user auth are not offered yet.
            AgentIdentity::Certificate { .. } => None,
        })
        .collect();
    if keys.is_empty() {
        return Err(SshError::Agent("the agent holds no usable key".into()));
    }
    // RSA keys are offered only with SHA-2 signatures (ADR-0011 §7).
    let mut rsa_hash: Option<Option<Option<HashAlg>>> = None;
    let mut tried = 0usize;
    for key in keys {
        if tried >= MAX_AGENT_IDENTITIES {
            break;
        }
        let hash_alg = if key.algorithm().is_rsa() {
            let h = match rsa_hash {
                Some(h) => h,
                None => {
                    let h = handle.best_supported_rsa_hash().await.ok().flatten();
                    rsa_hash = Some(h);
                    h
                }
            };
            match h {
                Some(Some(alg)) => Some(alg),
                // Server only offers ssh-rsa (SHA-1) or nothing: skip RSA keys.
                _ => continue,
            }
        } else if matches!(
            key.algorithm(),
            Algorithm::Ed25519 | Algorithm::Ecdsa { .. }
        ) {
            None
        } else {
            continue;
        };
        tried += 1;
        match handle
            .authenticate_publickey_with(hop.user, key, hash_alg, &mut agent)
            .await
        {
            Ok(AuthResult::Success) => return Ok(()),
            Ok(
                r @ AuthResult::Failure {
                    partial_success: true,
                    ..
                },
            ) => return hop.result(r, "agent"),
            Ok(AuthResult::Failure { .. }) => {}
            Err(e) => {
                tracing::debug!(hop = hop.index, error = %e, "agent signature failed; trying the next key");
            }
        }
    }
    Err(hop.failed("agent"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).
mod tests {
    use super::*;

    #[test]
    fn debug_redacts_secrets() {
        let a = HopAuth::Password(SecretString::from("hunter2-secret"));
        assert!(!format!("{a:?}").contains("hunter2"));
        let b = HopAuth::PublicKey {
            key_path: PathBuf::from("/k"),
            passphrase: Some(SecretString::from("pass-secret")),
        };
        let s = format!("{b:#?}");
        assert!(!s.contains("pass-secret"));
        assert!(s.contains("/k"));
        assert_eq!(a.method(), "password");
        assert_eq!(HopAuth::Agent.method(), "agent");
    }

    #[test]
    fn key_file_checks() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("id");
        std::fs::write(&p, "not a key").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            load_key(0, &p, None),
            Err(SshError::KeyFile {
                reason: KeyFileError::PermissionsTooOpen(0o644),
                ..
            })
        ));
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(matches!(
            load_key(0, &p, None),
            Err(SshError::KeyFile {
                reason: KeyFileError::Undecodable,
                ..
            })
        ));
        assert!(matches!(
            load_key(0, dir.path(), None),
            Err(SshError::KeyFile {
                reason: KeyFileError::NotAKeyFile,
                ..
            })
        ));
        assert!(matches!(
            load_key(0, &dir.path().join("missing"), None),
            Err(SshError::KeyFile {
                reason: KeyFileError::Unreadable(_),
                ..
            })
        ));
    }

    #[test]
    fn current_uid_matches_owned_files() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f");
        std::fs::write(&p, "x").unwrap();
        assert_eq!(current_uid(), Some(std::fs::metadata(&p).unwrap().uid()));
    }

    #[test]
    fn encrypted_key_needs_passphrase() {
        use russh::keys::ssh_key::LineEnding;
        let mut rng = rand::rng();
        let key = PrivateKey::random(&mut rng, Algorithm::Ed25519).unwrap();
        let enc = key.encrypt(&mut rng, "right-pass").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("id_ed25519");
        std::fs::write(&p, enc.to_openssh(LineEnding::LF).unwrap().as_bytes()).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(matches!(
            load_key(1, &p, None),
            Err(SshError::KeyFile {
                reason: KeyFileError::PassphraseRequired,
                ..
            })
        ));
        assert!(matches!(
            load_key(1, &p, Some(SecretString::from("wrong"))),
            Err(SshError::KeyFile {
                reason: KeyFileError::Undecodable,
                ..
            })
        ));
        let ok = load_key(1, &p, Some(SecretString::from("right-pass"))).unwrap();
        assert_eq!(ok.public_key(), key.public_key());
    }
}
