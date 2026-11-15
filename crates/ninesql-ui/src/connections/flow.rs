//! View models of everything the connect flow may ask or report (ROADMAP P1-C follow-ups):
//! secret prompts (`NeedsSecrets`), unknown SSH host keys (`HostKeyNeedsConfirmation`), init SQL
//! to confirm (`InitSqlNeedsConfirmation`), the production statement confirmation
//! (`ConfirmationRequest`), and the security warnings of an open session
//! (`SessionWarning`: driver warnings including `SshHopHostKeyNotVerified`, and every
//! `Tunnel::security_warnings` entry).
//!
//! They are plain data built from the core's values, so they are tested without a window, and
//! they never hold a secret, a parameter value or a result value.

use ninesql_core::driver_api::{
    SecurityWarning as ApiWarning, StatementClass, TlsMode, TunnelInfo,
};
use ninesql_core::ssh::{SshSecurityWarning, SshWarningKind, UnknownHostKey};
use ninesql_core::{ConfirmReason, ConfirmationRequest, SecretPrompt, SessionWarning};

/// What a `NeedsSecrets` prompt shows: one masked field per label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretPromptView {
    /// Labels, in the core's order (`Password of app on db.example.test`).
    pub labels: Vec<String>,
}

impl SecretPromptView {
    /// From the core's prompts.
    #[must_use]
    pub fn new(prompts: &[SecretPrompt]) -> Self {
        Self {
            labels: prompts.iter().map(|p| p.label.clone()).collect(),
        }
    }
}

/// What a `HostKeyNeedsConfirmation` dialog shows (ADR-0011 §6, ADR-0016).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostKeyView {
    /// `SSH hop 2 of the tunnel`.
    pub hop: String,
    /// `bastion.example.test:22`.
    pub address: String,
    /// `ssh-ed25519`.
    pub key_type: String,
    /// `SHA256:…`.
    pub fingerprint: String,
    /// When the host is known under other key types only (for example `ssh-rsa`, which NineSQL
    /// cannot use): the sentence to show. Never auto-accepted, not even under accept-new.
    pub other_key_types: Option<String>,
}

impl HostKeyView {
    /// From the key the core kept.
    #[must_use]
    pub fn new(key: &UnknownHostKey) -> Self {
        let address = if key.host.contains(':') {
            format!("[{}]:{}", key.host, key.port)
        } else {
            format!("{}:{}", key.host, key.port)
        };
        Self {
            hop: format!("SSH hop {}", key.hop + 1),
            address,
            key_type: key.key_type.clone(),
            fingerprint: key.fingerprint.to_string(),
            other_key_types: (!key.other_key_types.is_empty()).then(|| {
                format!(
                    "This host is known under another key type ({}), which NineSQL cannot use. Check with the server administrator that this new {} key is genuine before trusting it.",
                    key.other_key_types.join(", "),
                    key.key_type
                )
            }),
        }
    }
}

/// One init statement in the confirmation dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitSqlRow {
    /// Position, from 1.
    pub number: usize,
    /// The statement exactly as it will run.
    pub text: String,
    /// Why it needs confirmation, when flagged.
    pub flag: Option<String>,
}

/// What an `InitSqlNeedsConfirmation` dialog shows: every statement, flagged ones marked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitSqlView {
    /// Connection name.
    pub connection_name: String,
    /// The connection is typed production (colored frame).
    pub production: bool,
    /// Every statement, in order.
    pub rows: Vec<InitSqlRow>,
}

/// Short text of a statement class, for flags.
#[must_use]
pub fn class_text(class: StatementClass) -> &'static str {
    match class {
        StatementClass::Read => "read",
        StatementClass::Write => "write",
        StatementClass::Destructive => "destructive",
        StatementClass::Ddl => "schema change",
        StatementClass::Transaction => "transaction control",
        StatementClass::Session => "session setting",
        StatementClass::ChangesAccessMode => "changes the access mode",
        StatementClass::Unknown => "not recognized",
    }
}

fn reason_text(reason: ConfirmReason) -> &'static str {
    match reason {
        ConfirmReason::Write => "a write, and this connection type confirms every write",
        _ => "this connection type confirms destructive or unrecognized statements",
    }
}

impl InitSqlView {
    /// From the parts of an `InitSqlConfirmation`: `flagged` gives index, class and reason.
    #[must_use]
    pub fn new(
        connection_name: &str,
        production: bool,
        statements: &[String],
        flagged: impl IntoIterator<Item = (usize, StatementClass, ConfirmReason)>,
    ) -> Self {
        let mut rows: Vec<InitSqlRow> = statements
            .iter()
            .enumerate()
            .map(|(i, s)| InitSqlRow {
                number: i + 1,
                text: s.clone(),
                flag: None,
            })
            .collect();
        for (index, class, reason) in flagged {
            if let Some(row) = rows.get_mut(index) {
                row.flag = Some(format!("{}: {}", class_text(class), reason_text(reason)));
            }
        }
        Self {
            connection_name: connection_name.to_owned(),
            production,
            rows,
        }
    }

    /// From the core's confirmation.
    #[must_use]
    pub fn from_confirmation(c: &ninesql_core::InitSqlConfirmation) -> Self {
        Self::new(
            &c.connection_name,
            c.production,
            &c.statements,
            c.flagged.iter().map(|f| (f.index, f.class, f.reason)),
        )
    }

    /// Number of flagged statements.
    #[must_use]
    pub fn flagged(&self) -> usize {
        self.rows.iter().filter(|r| r.flag.is_some()).count()
    }
}

/// What the production statement confirmation shows (ADR-0007 §3): the exact statement, the
/// connection name in the type color, why it needs confirmation, and the number of bind
/// parameters (never their values).
///
/// INTEGRATION POINT (P3): statements are executed from the SQL editor in P3; the dialog
/// (`prompts::open_statement_confirmation`) is ready for `ExecEvent::NeedsConfirmation`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatementConfirmView {
    /// Connection name.
    pub connection_name: String,
    /// Typed production.
    pub production: bool,
    /// The statement exactly as it will run.
    pub statement: String,
    /// `destructive: this connection type confirms…`.
    pub reason: String,
    /// Number of bind parameters.
    pub params: usize,
}

impl StatementConfirmView {
    /// From the core's request.
    #[must_use]
    pub fn new(r: &ConfirmationRequest) -> Self {
        Self {
            connection_name: r.connection_name.clone(),
            production: r.production,
            statement: r.statement.clone(),
            reason: format!("{}: {}", class_text(r.class), reason_text(r.reason)),
            params: r.params,
        }
    }
}

fn tunnel_hop(tunnel: Option<&TunnelInfo>, hop: usize) -> String {
    let host = tunnel
        .and_then(|t| t.hops.get(hop))
        .map(|h| h.host.as_str());
    match host {
        Some(h) => format!("SSH hop {} ({h})", hop + 1),
        None => format!("SSH hop {}", hop + 1),
    }
}

/// Text of a warning of an open session, for its notification and badge tooltip.
#[must_use]
pub fn session_warning_text(w: &SessionWarning, tunnel: Option<&TunnelInfo>) -> String {
    match w {
        SessionWarning::Connection(api) => match api {
            ApiWarning::TlsDowngraded(TlsMode::Disable) => {
                "TLS is disabled on this session.".to_owned()
            }
            ApiWarning::TlsDowngraded(TlsMode::Require) => {
                "TLS encrypts this session but the server certificate is not verified.".to_owned()
            }
            ApiWarning::TlsDowngraded(_) => {
                "The server host name is not verified on this session.".to_owned()
            }
            ApiWarning::CredentialsInClear => {
                "Credentials were sent over the network without TLS.".to_owned()
            }
            ApiWarning::SshHostKeyNotVerified => {
                "An SSH hop of this session does not verify its host key.".to_owned()
            }
            ApiWarning::SshHopHostKeyNotVerified { hop } => format!(
                "{}: host key not verified.",
                tunnel_hop(tunnel, usize::from(*hop))
            ),
            ApiWarning::SshHostKeyAcceptNew => {
                "An SSH hop of this session accepts unknown host keys without asking.".to_owned()
            }
            ApiWarning::SshHopHostKeyAcceptNew { hop } => format!(
                "{}: unknown host keys are accepted without asking.",
                tunnel_hop(tunnel, usize::from(*hop))
            ),
            other => format!("{other:?}"),
        },
        SessionWarning::Ssh(s) => ssh_warning_text(s),
        SessionWarning::ReadOnlyClientSideOnly => {
            "Read-only is enforced by NineSQL only: this engine has no server-side read-only mode, so a statement NineSQL cannot classify could still write.".to_owned()
        }
        _ => "A security setting of this session is weakened.".to_owned(),
    }
}

/// Text of one `Tunnel::security_warnings` entry, including keys recorded automatically.
#[must_use]
pub fn ssh_warning_text(s: &SshSecurityWarning) -> String {
    let hop = format!("SSH hop {} ({}:{})", s.hop + 1, s.host, s.port);
    match &s.kind {
        SshWarningKind::HostKeyNotVerified => format!("{hop}: host key not verified."),
        SshWarningKind::AcceptNewHostKeys => {
            format!("{hop}: unknown host keys are accepted without asking.")
        }
        SshWarningKind::HostKeyAutoAccepted(fp) => format!(
            "{hop}: a new host key was recorded without confirmation: {fp}. Check it with the server administrator."
        ),
        _ => format!("{hop}: a security setting is weakened."),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).
mod tests {
    use ninesql_core::driver_api::{HostKeyVerification, SshHop as ApiHop};

    use super::*;

    fn key(other: Vec<String>) -> UnknownHostKey {
        UnknownHostKey {
            hop: 1,
            host: "bastion.example.test".into(),
            port: 2222,
            key_type: "ssh-ed25519".into(),
            fingerprint: ninesql_core::ssh::Fingerprint::of_blob(b"blob"),
            key_blob: b"blob".to_vec(),
            other_key_types: other,
        }
    }

    #[test]
    fn host_key_view_shows_hop_type_fingerprint_and_other_types() {
        let v = HostKeyView::new(&key(vec![]));
        assert_eq!(v.hop, "SSH hop 2");
        assert_eq!(v.address, "bastion.example.test:2222");
        assert_eq!(v.key_type, "ssh-ed25519");
        assert!(v.fingerprint.starts_with("SHA256:"));
        assert!(v.other_key_types.is_none());
        let v = HostKeyView::new(&key(vec!["ssh-rsa".into()]));
        assert!(v.other_key_types.unwrap().contains("ssh-rsa"));
    }

    #[test]
    fn init_sql_view_lists_every_statement_and_marks_flagged_ones() {
        let statements = vec![
            "SET search_path TO app".to_owned(),
            "DELETE FROM t".to_owned(),
        ];
        let v = InitSqlView::new(
            "Prod",
            true,
            &statements,
            [(1, StatementClass::Destructive, ConfirmReason::Destructive)],
        );
        assert_eq!(v.rows.len(), 2);
        assert!(v.rows[0].flag.is_none());
        assert!(
            v.rows[1]
                .flag
                .as_deref()
                .unwrap()
                .starts_with("destructive")
        );
        assert_eq!(v.flagged(), 1);
        assert!(v.production);
    }

    #[test]
    fn session_warnings_name_the_hop() {
        let mut tunnel = TunnelInfo::new("bastion", 22);
        tunnel.hops = vec![
            ApiHop::new("jump", 22, HostKeyVerification::Disabled),
            ApiHop::new("bastion", 22, HostKeyVerification::Strict),
        ];
        let w = SessionWarning::Connection(ApiWarning::SshHopHostKeyNotVerified { hop: 0 });
        assert_eq!(
            session_warning_text(&w, Some(&tunnel)),
            "SSH hop 1 (jump): host key not verified."
        );
        assert!(
            session_warning_text(&SessionWarning::ReadOnlyClientSideOnly, None)
                .contains("no server-side read-only")
        );
    }
}
