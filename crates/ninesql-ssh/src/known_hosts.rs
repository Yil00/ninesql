//! OpenSSH `known_hosts` parsing and matching (ADR-0011 §6, invariant I5).
//!
//! Two files are consulted:
//! - NineSQL's own file, `$XDG_DATA_HOME/ninesql/known_hosts`
//!   ([`ninesql_storage::Paths::known_hosts_file`]). It is the only file NineSQL writes: atomically
//!   (temp file + rename), mode `0600`, in a `0700` directory.
//! - Optionally the user's `~/.ssh/known_hosts`, **read-only**: NineSQL never modifies it.
//!
//! Supported format (sshd(8) "SSH_KNOWN_HOSTS FILE FORMAT"):
//! - comma-separated host patterns with `*` / `?` wildcards and `!` negation, matched
//!   case-insensitively;
//! - non-default ports written `[host]:port` (the pattern is matched against that string);
//! - hashed host names `|1|<base64 salt>|<base64 HMAC-SHA1>`;
//! - the `@revoked` marker (the key is refused for every host) and the `@cert-authority` marker
//!   (ignored: NineSQL does not negotiate host certificates, so such lines never vouch for a key);
//! - comments and blank lines.
//!
//! Parsing is total: a malformed line is skipped (and counted), never a panic or an error, because
//! the user's file may hold lines for key types or extensions this crate does not know.

use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use data_encoding::BASE64;
use hmac::{Hmac, KeyInit, Mac};
use ninesql_storage::DirLock;
use sha1::Sha1;

use crate::error::SshError;
use crate::fingerprint::Fingerprint;

/// Largest `known_hosts` file read (16 MiB). Larger files are refused, not truncated.
pub const MAX_KNOWN_HOSTS_SIZE: u64 = 16 * 1024 * 1024;

/// Default SSH port: a host on this port is looked up as `host`, otherwise as `[host]:port`.
pub const DEFAULT_SSH_PORT: u16 = 22;

/// Name of the workspace lock file, in the same directory as NineSQL's `known_hosts`
/// (`Paths::lock_file()` next to `Paths::known_hosts_file()`, both in `$XDG_DATA_HOME/ninesql`).
const LOCK_FILE_NAME: &str = "ninesql.lock";

/// Which file a `known_hosts` line comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum KnownHostsSource {
    /// NineSQL's own file (writable).
    NineSql,
    /// The user's `~/.ssh/known_hosts` (read-only).
    User,
}

/// Line marker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Marker {
    None,
    Revoked,
    CertAuthority,
}

/// One host pattern of a comma-separated list.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Pattern {
    negated: bool,
    /// Lower-cased glob.
    glob: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Hosts {
    Hashed { salt: Vec<u8>, hash: Vec<u8> },
    Patterns(Vec<Pattern>),
}

/// One parsed `known_hosts` line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KnownHostEntry {
    /// 1-based line number in its file.
    pub line: usize,
    marker: Marker,
    hosts: Hosts,
    /// Key type name as written (`ssh-ed25519`, `ecdsa-sha2-nistp256`…).
    pub key_type: String,
    /// Public key blob (SSH wire encoding).
    pub key_blob: Vec<u8>,
}

impl KnownHostEntry {
    /// Whether this line names `lookup` (see [`lookup_name`]).
    #[must_use]
    pub fn matches(&self, lookup: &str) -> bool {
        match &self.hosts {
            Hosts::Hashed { salt, hash } => {
                let Ok(mut mac) = Hmac::<Sha1>::new_from_slice(salt) else {
                    return false;
                };
                mac.update(lookup.as_bytes());
                // Constant-time comparison.
                mac.verify_slice(hash).is_ok()
            }
            Hosts::Patterns(patterns) => {
                let mut positive = false;
                for p in patterns {
                    if glob_match(&p.glob, lookup) {
                        if p.negated {
                            return false;
                        }
                        positive = true;
                    }
                }
                positive
            }
        }
    }

    /// `true` for a `@revoked` line.
    #[must_use]
    pub fn is_revoked(&self) -> bool {
        self.marker == Marker::Revoked
    }

    /// Fingerprint of the key on this line.
    #[must_use]
    pub fn fingerprint(&self) -> Fingerprint {
        Fingerprint::of_blob(&self.key_blob)
    }
}

/// Name under which a host is looked up: the lower-cased host for port 22, `[host]:port`
/// otherwise (the OpenSSH convention, also used as the input of hashed entries).
#[must_use]
pub fn lookup_name(host: &str, port: u16) -> String {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let host = host.to_ascii_lowercase();
    if port == DEFAULT_SSH_PORT {
        host
    } else {
        format!("[{host}]:{port}")
    }
}

/// OpenSSH-style glob: `*` matches any sequence (including empty), `?` exactly one character.
/// Both inputs are compared as given (callers lower-case them). Linear-time backtracking on the
/// last `*`, so hostile patterns cannot blow up.
fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        match p.get(pi) {
            Some('*') => {
                star = Some((pi, ti));
                pi += 1;
            }
            Some(&c) if c == '?' || c == t[ti] => {
                pi += 1;
                ti += 1;
            }
            _ => match star {
                Some((sp, st)) => {
                    pi = sp + 1;
                    ti = st + 1;
                    star = Some((sp, st + 1));
                }
                None => return false,
            },
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

/// Parsed content of one `known_hosts` file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KnownHostsFile {
    entries: Vec<KnownHostEntry>,
    skipped: usize,
}

impl KnownHostsFile {
    /// Parses the text of a `known_hosts` file. Never fails: malformed lines are skipped and
    /// counted in [`KnownHostsFile::skipped`].
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let mut out = Self::default();
        for (i, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            match parse_line(i + 1, line) {
                Some(entry) => out.entries.push(entry),
                None => out.skipped += 1,
            }
        }
        out
    }

    /// Parsed entries, in file order.
    #[must_use]
    pub fn entries(&self) -> &[KnownHostEntry] {
        &self.entries
    }

    /// Number of malformed or unsupported lines that were skipped.
    #[must_use]
    pub fn skipped(&self) -> usize {
        self.skipped
    }
}

fn parse_line(line_no: usize, line: &str) -> Option<KnownHostEntry> {
    let mut fields = line.split_ascii_whitespace();
    let mut first = fields.next()?;
    let marker = match first {
        "@revoked" => {
            first = fields.next()?;
            Marker::Revoked
        }
        "@cert-authority" => {
            first = fields.next()?;
            Marker::CertAuthority
        }
        m if m.starts_with('@') => return None,
        _ => Marker::None,
    };
    let key_type = fields.next()?;
    let key_b64 = fields.next()?;
    // The rest is a comment.
    let hosts = parse_hosts(first)?;
    let key_blob = BASE64.decode(key_b64.as_bytes()).ok()?;
    // The blob starts with its own key type: refuse lines where both disagree.
    if blob_key_type(&key_blob)? != key_type.as_bytes() {
        return None;
    }
    Some(KnownHostEntry {
        line: line_no,
        marker,
        hosts,
        key_type: key_type.to_owned(),
        key_blob,
    })
}

fn parse_hosts(field: &str) -> Option<Hosts> {
    if let Some(rest) = field.strip_prefix("|1|") {
        let (salt, hash) = rest.split_once('|')?;
        let salt = BASE64.decode(salt.as_bytes()).ok()?;
        let hash = BASE64.decode(hash.as_bytes()).ok()?;
        if salt.is_empty() || hash.len() != 20 {
            return None;
        }
        return Some(Hosts::Hashed { salt, hash });
    }
    if field.starts_with('|') {
        // Unknown hash format.
        return None;
    }
    let patterns: Vec<Pattern> = field
        .split(',')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (negated, glob) = match p.strip_prefix('!') {
                Some(g) => (true, g),
                None => (false, p),
            };
            Pattern {
                negated,
                glob: glob.to_ascii_lowercase(),
            }
        })
        .collect();
    if patterns.is_empty() {
        None
    } else {
        Some(Hosts::Patterns(patterns))
    }
}

/// Key type string at the start of an SSH public key blob.
fn blob_key_type(blob: &[u8]) -> Option<&[u8]> {
    let len_bytes: [u8; 4] = blob.get(..4)?.try_into().ok()?;
    let len = usize::try_from(u32::from_be_bytes(len_bytes)).ok()?;
    blob.get(4..4usize.checked_add(len)?)
}

/// Result of looking up a presented host key.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum HostKeyStatus {
    /// The exact key is known for this host.
    Known {
        /// File that vouches for it.
        file: KnownHostsSource,
        /// Line in that file.
        line: usize,
    },
    /// The host is known with a key of a type this client negotiates, but not this key:
    /// possible man-in-the-middle. Always a hard error (ADR-0011 §6).
    Changed {
        /// File holding the old key.
        file: KnownHostsSource,
        /// Line in that file.
        line: usize,
        /// Fingerprint of the old key.
        known_fingerprint: Fingerprint,
    },
    /// The key is listed as `@revoked`. Always a hard error.
    Revoked {
        /// File holding the `@revoked` line.
        file: KnownHostsSource,
        /// Line in that file.
        line: usize,
    },
    /// The host is not known (or only with key types this client cannot negotiate, listed in
    /// `other_key_types` so the UI can say so).
    Unknown {
        /// Key types known for the host that this client does not negotiate (for example
        /// `ssh-rsa` while RSA is disabled). Empty for a host never seen.
        other_key_types: Vec<String>,
    },
}

/// Key type names this client negotiates for host keys (see [`crate::tunnel`]).
pub(crate) const SUPPORTED_HOST_KEY_TYPES: &[&str] = &[
    "ssh-ed25519",
    "ecdsa-sha2-nistp256",
    "ecdsa-sha2-nistp384",
    "ecdsa-sha2-nistp521",
];

/// Looks a presented key up in `files` (in order). Pure function: no I/O.
///
/// Rules, strictest first:
/// 1. the key appears on a `@revoked` line of any file (whatever its host patterns) → `Revoked`;
/// 2. a plain line for the host holds this key → `Known`;
/// 3. a plain line for the host holds another key of a supported type → `Changed`;
/// 4. otherwise → `Unknown` (with the unsupported key types seen for the host, if any).
#[must_use]
pub fn check_host_key(
    files: &[(KnownHostsSource, &KnownHostsFile)],
    host: &str,
    port: u16,
    key_blob: &[u8],
) -> HostKeyStatus {
    let lookup = lookup_name(host, port);
    for (source, file) in files {
        if let Some(e) = file
            .entries
            .iter()
            .find(|e| e.marker == Marker::Revoked && e.key_blob == key_blob)
        {
            return HostKeyStatus::Revoked {
                file: *source,
                line: e.line,
            };
        }
    }
    let plain_for_host: Vec<(KnownHostsSource, &KnownHostEntry)> = files
        .iter()
        .flat_map(|(source, file)| {
            file.entries
                .iter()
                .filter(|e| e.marker == Marker::None && e.matches(&lookup))
                .map(move |e| (*source, e))
        })
        .collect();
    if let Some((file, e)) = plain_for_host.iter().find(|(_, e)| e.key_blob == key_blob) {
        return HostKeyStatus::Known {
            file: *file,
            line: e.line,
        };
    }
    if let Some((file, e)) = plain_for_host
        .iter()
        .find(|(_, e)| SUPPORTED_HOST_KEY_TYPES.contains(&e.key_type.as_str()))
    {
        return HostKeyStatus::Changed {
            file: *file,
            line: e.line,
            known_fingerprint: e.fingerprint(),
        };
    }
    let mut other_key_types: Vec<String> = plain_for_host
        .iter()
        .map(|(_, e)| e.key_type.clone())
        .collect();
    other_key_types.sort();
    other_key_types.dedup();
    HostKeyStatus::Unknown { other_key_types }
}

/// Supported key types known for a host, in file order without duplicates. Used to put them
/// first in the host key algorithm negotiation, so a known server presents the key on file
/// (what OpenSSH does too).
#[must_use]
pub fn known_key_types(
    files: &[(KnownHostsSource, &KnownHostsFile)],
    host: &str,
    port: u16,
) -> Vec<String> {
    let lookup = lookup_name(host, port);
    let mut out: Vec<String> = Vec::new();
    for (_, file) in files {
        for e in &file.entries {
            if e.marker == Marker::None
                && SUPPORTED_HOST_KEY_TYPES.contains(&e.key_type.as_str())
                && e.matches(&lookup)
                && !out.contains(&e.key_type)
            {
                out.push(e.key_type.clone());
            }
        }
    }
    out
}

/// Formats the line NineSQL appends for an accepted key: plain (not hashed) host name so the
/// user can review and remove entries, followed by the key and a `ninesql` comment.
#[must_use]
pub fn format_entry(host: &str, port: u16, key_type: &str, key_blob: &[u8]) -> String {
    format!(
        "{} {} {} ninesql\n",
        lookup_name(host, port),
        key_type,
        BASE64.encode(key_blob)
    )
}

/// The `known_hosts` files of one NineSQL installation.
///
/// Loading and saving do blocking file I/O: call them on the driver runtime
/// (`tokio::task::spawn_blocking`), never on the UI thread (I6). [`crate::tunnel::Tunnel::open`]
/// does so itself.
#[derive(Clone, Debug)]
pub struct KnownHosts {
    own_path: PathBuf,
    user_path: Option<PathBuf>,
    own: KnownHostsFile,
    user: KnownHostsFile,
}

impl KnownHosts {
    /// Loads NineSQL's file at `own_path` (missing = empty; group / other permission bits are
    /// removed) and, if given, the user's file at `user_path` read-only (missing = empty).
    ///
    /// # Errors
    /// [`SshError::KnownHosts`] if a file exists but cannot be read or is too large.
    pub fn load(own_path: &Path, user_path: Option<&Path>) -> Result<Self, SshError> {
        let own = read_own(own_path)?;
        let user = match user_path {
            Some(p) => read_user(p)?,
            None => KnownHostsFile::default(),
        };
        for (path, f) in [(Some(own_path), &own), (user_path, &user)] {
            if let (Some(path), true) = (path, f.skipped > 0) {
                tracing::debug!(
                    path = %path.display(),
                    skipped = f.skipped,
                    "skipped unsupported known_hosts lines"
                );
            }
        }
        Ok(Self {
            own_path: own_path.to_path_buf(),
            user_path: user_path.map(Path::to_path_buf),
            own,
            user,
        })
    }

    /// Path of the user's `~/.ssh/known_hosts` from `$HOME`, if `$HOME` is absolute.
    #[must_use]
    pub fn user_default_path() -> Option<PathBuf> {
        let home = std::env::var_os("HOME").map(PathBuf::from)?;
        home.is_absolute()
            .then(|| home.join(".ssh").join("known_hosts"))
    }

    /// Path of NineSQL's own file.
    #[must_use]
    pub fn own_path(&self) -> &Path {
        &self.own_path
    }

    /// Path of the user's file, if consulted.
    #[must_use]
    pub fn user_path(&self) -> Option<&Path> {
        self.user_path.as_deref()
    }

    fn files(&self) -> [(KnownHostsSource, &KnownHostsFile); 2] {
        [
            (KnownHostsSource::NineSql, &self.own),
            (KnownHostsSource::User, &self.user),
        ]
    }

    /// Looks a presented key up in both files (see [`check_host_key`]).
    #[must_use]
    pub fn check(&self, host: &str, port: u16, key_blob: &[u8]) -> HostKeyStatus {
        check_host_key(&self.files(), host, port, key_blob)
    }

    /// Supported key types on file for a host (see [`known_key_types`]).
    #[must_use]
    pub fn known_key_types(&self, host: &str, port: u16) -> Vec<String> {
        known_key_types(&self.files(), host, port)
    }

    /// Takes the workspace lock that guards writes to NineSQL's file across processes
    /// (ADR-0008 §3, review item L8): [`ninesql_storage::DirLock`] on [`LOCK_FILE_NAME`] next to
    /// the file, which for `Paths::known_hosts_file()` is `Paths::lock_file()`.
    ///
    /// A process that already holds the workspace lock (the core keeps it for the whole
    /// session) shares it: [`ninesql_storage::DirLock::acquire`] returns the held lock, so this
    /// never deadlocks against its own process. Within one process, [`crate::KnownHostsStore`]
    /// serializes writers. Never blocks: another NineSQL process holding the lock is an error.
    fn lock_own(&self) -> Result<Arc<DirLock>, SshError> {
        let dir = match self.own_path.parent() {
            Some(d) if !d.as_os_str().is_empty() => d,
            _ => Path::new("."),
        };
        ninesql_storage::fs::ensure_private_dir(dir)
            .map_err(|e| SshError::KnownHosts(e.to_string()))?;
        DirLock::acquire(&dir.join(LOCK_FILE_NAME)).map_err(|e| SshError::KnownHosts(e.to_string()))
    }

    /// Adds a key for `host:port` to NineSQL's file after the user confirmed it (or under the
    /// explicit accept-new policy). The read-modify-write runs under the workspace lock
    /// (`ninesql_storage::DirLock` on `Paths::lock_file()`, shared with the core if this process
    /// already holds it), so two NineSQL processes cannot drop each other's additions; the file is re-read first so earlier additions are kept, then
    /// rewritten atomically with mode `0600`. Adding a key that is already present for the
    /// host is a no-op. Refuses to add a key if the host has another supported key on file:
    /// a changed key must be removed explicitly first ([`KnownHosts::remove_host`], ADR-0011 §6).
    ///
    /// # Errors
    /// [`SshError::KnownHosts`] on I/O errors or when another NineSQL process holds the
    /// workspace lock, [`SshError::HostKeyChanged`] / [`SshError::HostKeyRevoked`] if the key
    /// cannot be added.
    pub fn add(
        &mut self,
        host: &str,
        port: u16,
        key_type: &str,
        key_blob: &[u8],
    ) -> Result<(), SshError> {
        validate_host(host)?;
        if blob_key_type(key_blob) != Some(key_type.as_bytes()) {
            return Err(SshError::InvalidHostKey);
        }
        let _lock = self.lock_own()?;
        self.own = read_own(&self.own_path)?;
        match self.check(host, port, key_blob) {
            HostKeyStatus::Known { .. } => return Ok(()),
            HostKeyStatus::Changed {
                file,
                line,
                known_fingerprint,
            } => {
                return Err(SshError::HostKeyChanged {
                    hop: None,
                    host: host.to_owned(),
                    port,
                    file,
                    line,
                    known_fingerprint,
                    presented_fingerprint: Fingerprint::of_blob(key_blob),
                });
            }
            HostKeyStatus::Revoked { file, line } => {
                return Err(SshError::HostKeyRevoked {
                    hop: None,
                    host: host.to_owned(),
                    port,
                    file,
                    line,
                });
            }
            HostKeyStatus::Unknown { .. } => {}
        }
        let mut text = read_text(&self.own_path, true)?.unwrap_or_default();
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&format_entry(host, port, key_type, key_blob));
        write_own(&self.own_path, &text)?;
        self.own = KnownHostsFile::parse(&text);
        tracing::info!(
            host,
            port,
            fingerprint = %Fingerprint::of_blob(key_blob),
            "added an SSH host key to NineSQL's known_hosts"
        );
        Ok(())
    }

    /// Removes every plain (non-marker) line of NineSQL's file that names `host:port`, including
    /// hashed lines and lines that list other hosts too (the whole line goes, like
    /// `ssh-keygen -R`). The user's `~/.ssh/known_hosts` is never modified: a key on file there
    /// must be removed by the user. Runs under the workspace lock, like [`KnownHosts::add`].
    /// Returns the number of lines removed.
    ///
    /// # Errors
    /// [`SshError::KnownHosts`] on I/O errors or when another NineSQL process holds the
    /// workspace lock.
    pub fn remove_host(&mut self, host: &str, port: u16) -> Result<usize, SshError> {
        let _lock = self.lock_own()?;
        let Some(text) = read_text(&self.own_path, true)? else {
            self.own = KnownHostsFile::default();
            return Ok(0);
        };
        let lookup = lookup_name(host, port);
        let mut kept = String::with_capacity(text.len());
        let mut removed = 0usize;
        for (i, raw) in text.lines().enumerate() {
            let trimmed = raw.trim();
            let drop = !trimmed.is_empty()
                && !trimmed.starts_with('#')
                && parse_line(i + 1, trimmed)
                    .is_some_and(|e| e.marker == Marker::None && e.matches(&lookup));
            if drop {
                removed += 1;
            } else {
                kept.push_str(raw);
                kept.push('\n');
            }
        }
        if removed > 0 {
            write_own(&self.own_path, &kept)?;
            tracing::info!(
                host,
                port,
                removed,
                "removed SSH host keys from NineSQL's known_hosts"
            );
        }
        self.own = KnownHostsFile::parse(&kept);
        Ok(removed)
    }
}

/// Host names (or IP addresses) accepted for SSH hops and written to the file: they cannot
/// inject patterns, extra fields or control characters.
pub(crate) fn is_valid_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 255
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':'))
}

fn validate_host(host: &str) -> Result<(), SshError> {
    if is_valid_host(host) {
        Ok(())
    } else {
        Err(SshError::InvalidHost)
    }
}

fn read_own(path: &Path) -> Result<KnownHostsFile, SshError> {
    Ok(read_text(path, true)?
        .map(|t| KnownHostsFile::parse(&t))
        .unwrap_or_default())
}

fn read_user(path: &Path) -> Result<KnownHostsFile, SshError> {
    Ok(read_text(path, false)?
        .map(|t| KnownHostsFile::parse(&t))
        .unwrap_or_default())
}

/// Reads a `known_hosts` file. NineSQL's own file goes through `ninesql-storage` (no symbolic
/// link, non-blocking open, permissions tightened). The user's file may legitimately be a
/// symbolic link (dotfile managers): it is followed, but must resolve to a regular file, and is
/// only read.
fn read_text(path: &Path, own: bool) -> Result<Option<String>, SshError> {
    let bytes = if own {
        ninesql_storage::fs::read_private(path, MAX_KNOWN_HOSTS_SIZE)
            .map_err(|e| SshError::KnownHosts(e.to_string()))?
    } else {
        read_user_bytes(path)
            .map_err(|e| SshError::KnownHosts(format!("cannot read {}: {e}", path.display())))?
    };
    // Invalid UTF-8 (unlikely in this format) is replaced; the affected lines then fail to parse.
    Ok(bytes.map(|b| String::from_utf8_lossy(&b).into_owned()))
}

fn read_user_bytes(path: &Path) -> io::Result<Option<Vec<u8>>> {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    if !meta.is_file() {
        return Err(io::Error::other("not a regular file"));
    }
    if meta.len() > MAX_KNOWN_HOSTS_SIZE {
        return Err(io::Error::other("file too large"));
    }
    let mut buf = Vec::new();
    File::open(path)?
        .take(MAX_KNOWN_HOSTS_SIZE)
        .read_to_end(&mut buf)?;
    Ok(Some(buf))
}

fn write_own(path: &Path, text: &str) -> Result<(), SshError> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        ninesql_storage::fs::ensure_private_dir(dir)
            .map_err(|e| SshError::KnownHosts(e.to_string()))?;
    }
    ninesql_storage::fs::write_atomic(path, text.as_bytes())
        .map_err(|e| SshError::KnownHosts(e.to_string()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).
mod tests {
    use super::*;
    use proptest::prelude::*;

    const K1: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl";
    const K2: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIBBBqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl";

    fn blob(k: &str) -> Vec<u8> {
        BASE64.decode(k.as_bytes()).unwrap()
    }

    fn rsa_blob() -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&7u32.to_be_bytes());
        b.extend_from_slice(b"ssh-rsa");
        b.extend_from_slice(&[0, 0, 0, 1, 3, 0, 0, 0, 1, 1]);
        b
    }

    fn hashed(host: &str, salt: &[u8]) -> String {
        let mut mac = Hmac::<Sha1>::new_from_slice(salt).unwrap();
        mac.update(host.as_bytes());
        let h = mac.finalize().into_bytes();
        format!("|1|{}|{}", BASE64.encode(salt), BASE64.encode(&h))
    }

    fn check(text: &str, host: &str, port: u16, key: &[u8]) -> HostKeyStatus {
        let f = KnownHostsFile::parse(text);
        check_host_key(&[(KnownHostsSource::User, &f)], host, port, key)
    }

    #[test]
    fn lookup_names() {
        assert_eq!(
            lookup_name("Bastion.Example.TEST", 22),
            "bastion.example.test"
        );
        assert_eq!(lookup_name("db", 2222), "[db]:2222");
        assert_eq!(lookup_name("::1", 2222), "[::1]:2222");
        assert_eq!(lookup_name("[::1]", 22), "::1");
    }

    #[test]
    fn glob() {
        assert!(glob_match("*", ""));
        assert!(glob_match("*.example.test", "a.b.example.test"));
        assert!(!glob_match("*.example.test", "example.test"));
        assert!(glob_match("db?", "db1"));
        assert!(!glob_match("db?", "db"));
        assert!(glob_match("[db]:*", "[db]:2222"));
        assert!(glob_match("a*b*c", "axxbyyc"));
        assert!(!glob_match("a*b*c", "axxbyy"));
        assert!(glob_match("**", "anything"));
    }

    #[test]
    fn plain_match_and_case() {
        let t = format!("Bastion.example.test,10.0.0.1 ssh-ed25519 {K1} comment here\n");
        assert!(matches!(
            check(&t, "bastion.EXAMPLE.test", 22, &blob(K1)),
            HostKeyStatus::Known { line: 1, .. }
        ));
        assert!(matches!(
            check(&t, "10.0.0.1", 22, &blob(K1)),
            HostKeyStatus::Known { .. }
        ));
    }

    #[test]
    fn port_must_match() {
        let t = format!("[db.example.test]:2222 ssh-ed25519 {K1}\n");
        assert!(matches!(
            check(&t, "db.example.test", 2222, &blob(K1)),
            HostKeyStatus::Known { .. }
        ));
        // Same host on the default port is a different entry.
        assert_eq!(
            check(&t, "db.example.test", 22, &blob(K1)),
            HostKeyStatus::Unknown {
                other_key_types: vec![]
            }
        );
        // A port-22 entry does not cover another port.
        let t22 = format!("db.example.test ssh-ed25519 {K1}\n");
        assert!(matches!(
            check(&t22, "db.example.test", 2222, &blob(K1)),
            HostKeyStatus::Unknown { .. }
        ));
    }

    #[test]
    fn wildcards_and_negation() {
        let t = format!("*.example.test,!evil.example.test ssh-ed25519 {K1}\n");
        assert!(matches!(
            check(&t, "db.example.test", 22, &blob(K1)),
            HostKeyStatus::Known { .. }
        ));
        assert!(matches!(
            check(&t, "evil.example.test", 22, &blob(K1)),
            HostKeyStatus::Unknown { .. }
        ));
        let tp = format!("[*.example.test]:* ssh-ed25519 {K1}\n");
        assert!(matches!(
            check(&tp, "db.example.test", 2200, &blob(K1)),
            HostKeyStatus::Known { .. }
        ));
    }

    #[test]
    fn hashed_hosts() {
        let salt = b"0123456789abcdefghij";
        let t = format!(
            "{} ssh-ed25519 {K1}\n{} ssh-ed25519 {K2}\n",
            hashed("bastion.example.test", salt),
            hashed("[db.example.test]:2222", salt)
        );
        assert!(matches!(
            check(&t, "Bastion.example.test", 22, &blob(K1)),
            HostKeyStatus::Known { line: 1, .. }
        ));
        assert!(matches!(
            check(&t, "db.example.test", 2222, &blob(K2)),
            HostKeyStatus::Known { line: 2, .. }
        ));
        assert!(matches!(
            check(&t, "db.example.test", 22, &blob(K2)),
            HostKeyStatus::Unknown { .. }
        ));
    }

    #[test]
    fn changed_key_is_reported() {
        let t = format!("db.example.test ssh-ed25519 {K1}\n");
        match check(&t, "db.example.test", 22, &blob(K2)) {
            HostKeyStatus::Changed {
                line,
                known_fingerprint,
                ..
            } => {
                assert_eq!(line, 1);
                assert_eq!(known_fingerprint, Fingerprint::of_blob(&blob(K1)));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn unsupported_known_type_is_unknown_with_note() {
        let t = format!("db.example.test ssh-rsa {}\n", BASE64.encode(&rsa_blob()));
        assert_eq!(
            check(&t, "db.example.test", 22, &blob(K1)),
            HostKeyStatus::Unknown {
                other_key_types: vec!["ssh-rsa".to_owned()]
            }
        );
    }

    #[test]
    fn revoked_wins_for_any_host() {
        let t = format!(
            "db.example.test ssh-ed25519 {K1}\n@revoked other.example.test ssh-ed25519 {K1}\n"
        );
        assert!(matches!(
            check(&t, "db.example.test", 22, &blob(K1)),
            HostKeyStatus::Revoked { line: 2, .. }
        ));
    }

    #[test]
    fn cert_authority_lines_never_vouch() {
        let t = format!("@cert-authority *.example.test ssh-ed25519 {K1}\n");
        assert!(matches!(
            check(&t, "db.example.test", 22, &blob(K1)),
            HostKeyStatus::Unknown { .. }
        ));
    }

    #[test]
    fn malformed_lines_are_skipped() {
        let t = format!(
            "# comment\n\n@unknown x ssh-ed25519 {K1}\nonlyhost\nh ssh-ed25519 !!notbase64\n\
             h ssh-rsa {K1}\n|2|abc|def ssh-ed25519 {K1}\n|1|bad ssh-ed25519 {K1}\n\
             good ssh-ed25519 {K1}\n"
        );
        let f = KnownHostsFile::parse(&t);
        assert_eq!(f.entries().len(), 1);
        assert_eq!(f.skipped(), 6);
        assert_eq!(f.entries()[0].line, 9);
    }

    #[test]
    fn known_types_order() {
        let t = format!(
            "db ssh-rsa {}\ndb ecdsa-sha2-nistp256 {}\ndb ssh-ed25519 {K1}\n",
            BASE64.encode(&rsa_blob()),
            BASE64.encode(&ecdsa_blob())
        );
        let f = KnownHostsFile::parse(&t);
        assert_eq!(
            known_key_types(&[(KnownHostsSource::User, &f)], "db", 22),
            vec!["ecdsa-sha2-nistp256".to_owned(), "ssh-ed25519".to_owned()]
        );
    }

    fn ecdsa_blob() -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&19u32.to_be_bytes());
        b.extend_from_slice(b"ecdsa-sha2-nistp256");
        b.extend_from_slice(&[0, 0, 0, 1, 0]);
        b
    }

    #[test]
    fn own_file_add_remove_atomic_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let own = dir.path().join("ninesql").join("known_hosts");
        let user = dir.path().join("user_known_hosts");
        let user_text = format!("other.example.test ssh-ed25519 {K2}\n");
        std::fs::write(&user, &user_text).unwrap();

        let mut kh = KnownHosts::load(&own, Some(&user)).unwrap();
        assert!(matches!(
            kh.check("db.example.test", 2222, &blob(K1)),
            HostKeyStatus::Unknown { .. }
        ));
        kh.add("db.example.test", 2222, "ssh-ed25519", &blob(K1))
            .unwrap();
        // Idempotent.
        kh.add("db.example.test", 2222, "ssh-ed25519", &blob(K1))
            .unwrap();
        let text = std::fs::read_to_string(&own).unwrap();
        assert_eq!(text.lines().count(), 1);
        assert!(text.starts_with("[db.example.test]:2222 ssh-ed25519 "));
        let mode = std::fs::metadata(&own).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let dmode = std::fs::metadata(own.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dmode, 0o700);

        // Reload from disk sees it, with the right source.
        let mut kh = KnownHosts::load(&own, Some(&user)).unwrap();
        assert_eq!(
            kh.check("db.example.test", 2222, &blob(K1)),
            HostKeyStatus::Known {
                file: KnownHostsSource::NineSql,
                line: 1
            }
        );
        assert!(matches!(
            kh.check("other.example.test", 22, &blob(K2)),
            HostKeyStatus::Known {
                file: KnownHostsSource::User,
                ..
            }
        ));

        // A changed key cannot be added over the old one.
        assert!(matches!(
            kh.add("db.example.test", 2222, "ssh-ed25519", &blob(K2)),
            Err(SshError::HostKeyChanged { .. })
        ));
        // Nor over a key in the user's file (which is never modified).
        assert!(matches!(
            kh.add("other.example.test", 22, "ssh-ed25519", &blob(K1)),
            Err(SshError::HostKeyChanged { .. })
        ));
        assert_eq!(kh.remove_host("other.example.test", 22).unwrap(), 0);
        assert_eq!(std::fs::read_to_string(&user).unwrap(), user_text);

        // Explicit removal, then the new key can be added.
        assert_eq!(kh.remove_host("db.example.test", 2222).unwrap(), 1);
        kh.add("db.example.test", 2222, "ssh-ed25519", &blob(K2))
            .unwrap();
        assert!(matches!(
            kh.check("db.example.test", 2222, &blob(K2)),
            HostKeyStatus::Known { .. }
        ));
    }

    #[test]
    fn lock_file_is_the_workspace_lock() {
        let xdg = ninesql_storage::XdgDirs::from_lookup(|k| {
            (k == "HOME").then(|| std::ffi::OsString::from("/home/u"))
        })
        .unwrap();
        let paths = ninesql_storage::Paths::new(&xdg);
        assert_eq!(
            paths.known_hosts_file().with_file_name(LOCK_FILE_NAME),
            paths.lock_file()
        );
    }

    #[test]
    fn writes_are_guarded_by_the_workspace_lock() {
        use std::fs::{File, OpenOptions};
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("ninesql");
        let own = data.join("known_hosts");
        let lock_path = data.join(LOCK_FILE_NAME);
        let mut kh = KnownHosts::load(&own, None).unwrap();

        // This process already holds the workspace lock (as the core does for the session):
        // writes share it instead of deadlocking or failing.
        ninesql_storage::fs::ensure_private_dir(&data).unwrap();
        let held = DirLock::acquire(&lock_path).unwrap();
        kh.add("a.example.test", 22, "ssh-ed25519", &blob(K1))
            .unwrap();
        assert_eq!(kh.remove_host("a.example.test", 22).unwrap(), 1);
        drop(held);
        // Released once the writes are done.
        assert!(!ninesql_storage::lock::is_locked_elsewhere(&lock_path).unwrap());

        // Another process (another open file description) holds the lock: the write is refused
        // and the file is left untouched.
        kh.add("b.example.test", 22, "ssh-ed25519", &blob(K1))
            .unwrap();
        let before = std::fs::read_to_string(&own).unwrap();
        let other: File = OpenOptions::new().read(true).open(&lock_path).unwrap();
        other.try_lock().unwrap();
        let err = kh
            .add("c.example.test", 22, "ssh-ed25519", &blob(K2))
            .unwrap_err();
        assert!(
            matches!(&err, SshError::KnownHosts(m) if m.contains("locked")),
            "{err:?}"
        );
        assert!(matches!(
            kh.remove_host("b.example.test", 22),
            Err(SshError::KnownHosts(_))
        ));
        assert_eq!(std::fs::read_to_string(&own).unwrap(), before);
        drop(other);
        kh.add("c.example.test", 22, "ssh-ed25519", &blob(K2))
            .unwrap();
        assert_eq!(std::fs::read_to_string(&own).unwrap().lines().count(), 2);
    }

    #[test]
    fn add_rejects_injection_and_mismatched_blob() {
        let dir = tempfile::tempdir().unwrap();
        let own = dir.path().join("known_hosts");
        let mut kh = KnownHosts::load(&own, None).unwrap();
        for bad in ["*", "a,b", "a b", "!x", "|1|x", "", "h\nevil ssh-ed25519"] {
            assert!(
                matches!(
                    kh.add(bad, 22, "ssh-ed25519", &blob(K1)),
                    Err(SshError::InvalidHost)
                ),
                "{bad:?}"
            );
        }
        assert!(matches!(
            kh.add("h", 22, "ecdsa-sha2-nistp256", &blob(K1)),
            Err(SshError::InvalidHostKey)
        ));
        assert!(!own.exists());
    }

    #[test]
    fn user_file_symlink_is_followed_read_only() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::write(&real, format!("h ssh-ed25519 {K1}\n")).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let kh = KnownHosts::load(&dir.path().join("own"), Some(&link)).unwrap();
        assert!(matches!(
            kh.check("h", 22, &blob(K1)),
            HostKeyStatus::Known { .. }
        ));
        // But NineSQL's own file must not be a symbolic link.
        assert!(KnownHosts::load(&link, None).is_err());
    }

    proptest! {
        #[test]
        fn parse_never_panics(text in "\\PC{0,400}") {
            let f = KnownHostsFile::parse(&text);
            let _ = check_host_key(&[(KnownHostsSource::User, &f)], "h", 22, b"x");
        }

        #[test]
        fn glob_star_matches_everything(s in "[a-z0-9.:\\[\\]-]{0,40}") {
            prop_assert!(glob_match("*", &s));
            prop_assert!(glob_match(&s, &s));
        }

        #[test]
        fn hashed_entry_matches_only_its_host(host in "[a-z0-9.-]{1,30}", other in "[a-z0-9.-]{1,30}", port in 1u16..) {
            let name = lookup_name(&host, port);
            let line = format!("{} ssh-ed25519 {K1}\n", hashed(&name, b"salt-salt-salt-salt!"));
            let f = KnownHostsFile::parse(&line);
            prop_assert!(f.entries()[0].matches(&name));
            let other_name = lookup_name(&other, port);
            prop_assert_eq!(f.entries()[0].matches(&other_name), other_name == name);
        }
    }
}
