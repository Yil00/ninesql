//! Classification of the DBeaver keys the mapping does not consume.
//!
//! Every key of `data-sources.json` is either mapped (consumed by [`crate::mapping`]), listed here
//! as **ignored** with a reason (no report entry: nothing the user relies on is lost) or as
//! **unsupported** (a report entry), or it is **unknown** (a report entry "not imported"). The
//! ignore reasons are mirrored in docs/06-dbeaver-interop.md.

use crate::model::Extra;
use crate::report::{IssueKind, Issues};

/// Object of `data-sources.json` an unconsumed key belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scope {
    TopLevel,
    Folder,
    ConnectionType,
    Connection,
    Configuration,
    Bootstrap,
    Handler,
    SshProperties,
    PostgresSslProperties,
    MysqlSslProperties,
}

/// What to do with an unconsumed key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Class {
    /// Dropped on purpose, no report entry.
    Ignored(&'static str),
    /// Known setting without a NineSQL equivalent: reported.
    Unsupported(&'static str),
    /// Not known: reported as not imported.
    Unknown,
}

pub(crate) fn classify(scope: Scope, key: &str) -> Class {
    use Class::{Ignored, Unknown, Unsupported};
    match (scope, key) {
        (Scope::TopLevel, "network-profiles") => {
            Unsupported("network profiles are not supported yet (F-CON-17)")
        }
        (Scope::TopLevel, "virtual-models") => {
            Unsupported("virtual keys and references have no NineSQL equivalent")
        }
        (Scope::Connection, k) if k.starts_with("navigator-") => {
            Unsupported("navigator display settings have no NineSQL equivalent")
        }
        (Scope::Configuration, "closeIdleConnection") => Ignored(
            "written on every connection by DBeaver 23.x to 25.x; NineSQL has no per-connection \
             idle-close setting yet (F-CON-10)",
        ),
        (Scope::Configuration, "events") => Unsupported(
            "shell commands run on connection events are not imported (NineSQL never runs \
             commands from an imported file)",
        ),
        (Scope::Configuration, "home") => {
            Unsupported("the local client home (native dump / restore tools) is not imported")
        }
        (Scope::Configuration, "auth-properties") => {
            Unsupported("authentication model properties are not imported")
        }
        (Scope::Bootstrap, "txnIsolation") => {
            Unsupported("a default transaction isolation level is not supported yet")
        }
        (Scope::Bootstrap, "defaultObject") => Unsupported(
            "legacy default-object setting: DBeaver reads it only when `defaultSchema` is empty, \
             through an indirection NineSQL does not reproduce",
        ),
        (Scope::Bootstrap, "ignoreErrors") => {
            Unsupported("NineSQL stops at the first failing init statement")
        }
        (Scope::SshProperties, "implementation") => {
            Ignored("DBeaver's SSH library choice; NineSQL uses its own SSH client (ADR-0011)")
        }
        (Scope::SshProperties, "localHost" | "localPort") => Unsupported(
            "a fixed local forwarding address is not supported: NineSQL binds 127.0.0.1 on an \
             ephemeral port (ADR-0011)",
        ),
        (Scope::SshProperties, "aliveInterval" | "aliveCount" | "sshConnectTimeout") => {
            Unsupported("SSH keep-alive and timeout settings are not imported")
        }
        (Scope::SshProperties, k) if is_host_key_bypass(k) => Unsupported(
            "NineSQL always verifies SSH host keys; the first connection asks to confirm the key \
             (ADR-0011, I5)",
        ),
        (Scope::PostgresSslProperties, "ssl.method") => Ignored(
            "selects certificate files in DBeaver's dialog; the paths are imported directly",
        ),
        (Scope::PostgresSslProperties | Scope::MysqlSslProperties, "sslFactory") => {
            Unsupported("custom Java SSL socket factories have no NineSQL equivalent")
        }
        (Scope::MysqlSslProperties, "ssl.cipher.suites") => {
            Unsupported("TLS cipher suite selection is not supported (rustls defaults)")
        }
        _ => Unknown,
    }
}

fn is_host_key_bypass(key: &str) -> bool {
    let k: String = key
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect();
    k.contains("hostverif") || k.contains("stricthost") || k.contains("hostkey")
}

/// Reports every key of `extra` according to [`classify`]; `prefix` is the key path of the
/// object (`configuration.bootstrap`), empty at the top level.
pub(crate) fn report_extra(scope: Scope, prefix: &str, extra: &Extra, issues: &mut Issues) {
    for key in extra.keys() {
        report_key(scope, prefix, key, issues);
    }
}

pub(crate) fn report_key(scope: Scope, prefix: &str, key: &str, issues: &mut Issues) {
    let path = join(prefix, key);
    match classify(scope, key) {
        Class::Ignored(_) => {}
        Class::Unsupported(reason) => issues.warn(
            IssueKind::Unsupported,
            Some(path),
            format!("not imported: {reason}"),
        ),
        Class::Unknown => issues.warn(
            IssueKind::UnknownKey,
            Some(path),
            "not imported: unknown DBeaver setting (or unexpected value type)",
        ),
    }
}

pub(crate) fn join(prefix: &str, key: &str) -> String {
    if prefix.is_empty() {
        key.to_owned()
    } else {
        format!("{prefix}.{key}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classification() {
        assert!(matches!(
            classify(Scope::SshProperties, "implementation"),
            Class::Ignored(_)
        ));
        assert!(matches!(
            classify(Scope::SshProperties, "bypassHostVerification"),
            Class::Unsupported(_)
        ));
        assert!(matches!(
            classify(Scope::Connection, "navigator-show-only-entities"),
            Class::Unsupported(_)
        ));
        assert_eq!(classify(Scope::Folder, "whatever"), Class::Unknown);
    }
}
