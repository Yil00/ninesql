//! Import report entries (ADR-0006 §6, docs/06-dbeaver-interop.md "Import report").
//!
//! An issue names DBeaver keys, ids and static explanations. It **never** contains a value read
//! from a DBeaver file other than identifiers (connection ids, provider / driver ids, folder and
//! connection type ids): no host, no user, no property value, no secret (I1).

use std::fmt;

/// How serious an issue is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Severity {
    /// For information: something was converted or a default applied.
    Info,
    /// Something was not imported or was weakened; the connection still works.
    Warning,
    /// The connection could not be imported (or a whole file could not be read).
    Error,
}

/// What an issue is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum IssueKind {
    /// A DBeaver key NineSQL does not know: kept in the parsed model's `extra`, not imported.
    UnknownKey,
    /// A known DBeaver setting with no NineSQL equivalent (yet): not imported.
    Unsupported,
    /// A value had an unexpected type or was out of range: not imported.
    InvalidValue,
    /// A driver property reserved for NineSQL's typed settings (driver API rule): not imported.
    ReservedProperty,
    /// A secret found where it does not belong (driver property, URL): removed, not imported.
    SecretRemoved,
    /// A secret stored in clear in a DBeaver file: moved to the secret store like the others.
    PlaintextSecret,
    /// The engine is outside NineSQL's v1 set or unknown: imported disabled (ADR-0006 §5).
    UnsupportedEngine,
    /// The connection is imported disabled for another reason (unsupported proxy, tunnel…).
    Disabled,
    /// TLS does not verify the server (or is off): explicit, flagged downgrade (I5).
    TlsNotVerified,
    /// A TLS mode was converted to the nearest NineSQL mode, or a default applied.
    TlsConverted,
    /// The SSH host key is not trusted on import: the first connection asks (ADR-0011 §6).
    SshHostKeyToConfirm,
    /// Saved credentials could not be read or matched.
    Credentials,
    /// A data-sources file could not be read or parsed.
    File,
}

/// One entry of the import report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportIssue {
    /// Severity.
    pub severity: Severity,
    /// Category.
    pub kind: IssueKind,
    /// DBeaver key path concerned (`configuration.handlers.ssh_tunnel.properties.aliveInterval`),
    /// when there is one.
    pub key: Option<String>,
    /// User-presentable explanation (no value from the file, see the module documentation).
    pub message: String,
}

impl ImportIssue {
    pub(crate) fn new(
        severity: Severity,
        kind: IssueKind,
        key: Option<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            severity,
            kind,
            key: key.map(|k| ident(&k, MAX_KEY_PATH)),
            message: message.into(),
        }
    }
}

/// Longest key path kept in an issue, in bytes.
const MAX_KEY_PATH: usize = 512;
/// Longest identifier embedded in an issue message, in bytes.
pub(crate) const MAX_IDENT: usize = 96;

/// An identifier read from a DBeaver file, made safe to show: control characters replaced by
/// `?`, truncated to `max` bytes (with `…`). Keys and ids come from untrusted files.
pub(crate) fn ident(s: &str, max: usize) -> String {
    let clean: String = s
        .chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect();
    if clean.len() <= max {
        return clean;
    }
    let mut end = max;
    while !clean.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &clean[..end])
}

impl fmt::Display for ImportIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.key {
            Some(key) => write!(f, "`{key}`: {}", self.message),
            None => f.write_str(&self.message),
        }
    }
}

/// Collects issues for one scope.
#[cfg(test)]
mod tests {
    #[test]
    fn identifiers_are_sanitized() {
        assert_eq!(super::ident("a\nb", 10), "a?b");
        assert_eq!(super::ident("ééé", 3), "é…");
    }
}

#[derive(Debug, Default)]
pub(crate) struct Issues(pub(crate) Vec<ImportIssue>);

impl Issues {
    pub(crate) fn push(
        &mut self,
        severity: Severity,
        kind: IssueKind,
        key: Option<String>,
        message: impl Into<String>,
    ) {
        self.0.push(ImportIssue::new(severity, kind, key, message));
    }

    pub(crate) fn info(&mut self, kind: IssueKind, key: Option<String>, msg: impl Into<String>) {
        self.push(Severity::Info, kind, key, msg);
    }

    pub(crate) fn warn(&mut self, kind: IssueKind, key: Option<String>, msg: impl Into<String>) {
        self.push(Severity::Warning, kind, key, msg);
    }

    pub(crate) fn error(&mut self, kind: IssueKind, key: Option<String>, msg: impl Into<String>) {
        self.push(Severity::Error, kind, key, msg);
    }
}
