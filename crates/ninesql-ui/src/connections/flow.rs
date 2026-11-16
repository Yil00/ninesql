//! View models of everything the connect flow may ask or report (ROADMAP P1-C follow-ups):
//! secret prompts (`NeedsSecrets`), unknown SSH host keys (`HostKeyNeedsConfirmation`), init SQL
//! to confirm (`InitSqlNeedsConfirmation`), the production statement confirmation
//! (`ConfirmationRequest`), and the security warnings of an open session
//! (`SessionWarning`: driver warnings including `SshHopHostKeyNotVerified`, and every
//! `Tunnel::security_warnings` entry).
//!
//! They are plain data built from the core's values, so they are tested without a window, and
//! they never hold a secret, a parameter value or a result value.

use gpui_kit::SharedString;
use ninesql_core::driver_api::{
    SecurityWarning as ApiWarning, StatementClass, TlsMode, TunnelInfo,
};
use ninesql_core::ssh::{SshSecurityWarning, SshWarningKind, UnknownHostKey};
use ninesql_core::{ConfirmReason, ConfirmationRequest, SecretPrompt, SessionWarning};

/// Statements up to this size are shown whole in the confirmation; larger ones show their head
/// and their tail (the UI thread never lays out a multi-megabyte statement).
pub const STATEMENT_PREVIEW_MAX_BYTES: usize = 64 * 1024;
/// Bytes of the head, and of the tail, shown of a statement over
/// [`STATEMENT_PREVIEW_MAX_BYTES`].
pub const STATEMENT_PREVIEW_PART_BYTES: usize = 24 * 1024;
/// Characters of the last line shown under the statement box.
pub const STATEMENT_LAST_LINE_CHARS: usize = 200;

/// A statement as the confirmation shows it (ADR-0007 §3, phase 3 review L1): bounded, and its
/// end always visible, since a destructive tail can hide after a long harmless head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatementPreview {
    /// The whole statement, or its first [`STATEMENT_PREVIEW_PART_BYTES`].
    pub head: SharedString,
    /// Bytes between the head and the tail that are not shown (0: the whole statement is in
    /// `head`).
    pub hidden_bytes: usize,
    /// The last [`STATEMENT_PREVIEW_PART_BYTES`] when bytes are hidden, else empty.
    pub tail: SharedString,
    /// Lines of the whole statement.
    pub lines: usize,
    /// Bytes of the whole statement.
    pub bytes: usize,
    /// End of the last non-blank line (at most [`STATEMENT_LAST_LINE_CHARS`] characters,
    /// `…` in front when cut), shown under the box whatever its scroll position.
    pub last_line: SharedString,
}

fn floor_boundary(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_boundary(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

impl StatementPreview {
    /// The preview of `text`.
    #[must_use]
    pub fn of(text: &str) -> Self {
        let bytes = text.len();
        let lines = text.bytes().filter(|b| *b == b'\n').count() + 1;
        let (head, hidden_bytes, tail) = if bytes > STATEMENT_PREVIEW_MAX_BYTES {
            let head_end = floor_boundary(text, STATEMENT_PREVIEW_PART_BYTES);
            let tail_start = ceil_boundary(text, bytes - STATEMENT_PREVIEW_PART_BYTES);
            (
                &text[..head_end],
                tail_start - head_end,
                &text[tail_start..],
            )
        } else {
            (text, 0, "")
        };
        let last = text
            .trim_end()
            .rsplit('\n')
            .next()
            .unwrap_or_default()
            .trim_start();
        let count = last.chars().count();
        let last_line = if count > STATEMENT_LAST_LINE_CHARS {
            let skip = last
                .char_indices()
                .nth(count - STATEMENT_LAST_LINE_CHARS)
                .map_or(0, |(i, _)| i);
            format!("…{}", &last[skip..])
        } else {
            last.to_owned()
        };
        Self {
            head: SharedString::from(head.to_owned()),
            hidden_bytes,
            tail: SharedString::from(tail.to_owned()),
            lines,
            bytes,
            last_line: SharedString::from(last_line),
        }
    }

    /// `… 1048576 bytes not shown …`, between the head and the tail of a large statement.
    #[must_use]
    pub fn hidden_marker(&self) -> Option<String> {
        (self.hidden_bytes > 0).then(|| format!("… {} bytes not shown …", self.hidden_bytes))
    }

    /// `12 lines · 340 bytes`.
    #[must_use]
    pub fn size_text(&self) -> String {
        let lines = if self.lines == 1 { "line" } else { "lines" };
        let bytes = if self.bytes == 1 { "byte" } else { "bytes" };
        format!("{} {lines} · {} {bytes}", self.lines, self.bytes)
    }

    /// Whether the end of the statement may be out of sight in the box (long or cut), so the
    /// last line is repeated under it.
    #[must_use]
    pub fn needs_last_line(&self) -> bool {
        self.hidden_bytes > 0 || self.lines > STATEMENT_BOX_LINES || self.bytes > 2000
    }
}

/// Lines the statement box shows before it scrolls (its maximum height).
pub const STATEMENT_BOX_LINES: usize = 16;

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
    /// The init SQL was imported (a DBeaver workspace) and is not confirmed yet
    /// (`InitSqlConfirmation::imported`): the dialog shows `IMPORTED_INIT_SQL_NOTICE`.
    pub imported: bool,
    /// Every statement, in order.
    pub rows: Vec<InitSqlRow>,
}

/// Notice of the init SQL dialog when the statements were imported and are not confirmed yet.
pub const IMPORTED_INIT_SQL_NOTICE: &str = "These init statements were imported and have not been confirmed yet; they run on every connect once you confirm.";

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
        ConfirmReason::Imported => {
            "imported and not confirmed yet; it runs at every connect once you confirm"
        }
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
            imported: false,
            rows,
        }
    }

    /// From the core's confirmation.
    #[must_use]
    pub fn from_confirmation(c: &ninesql_core::InitSqlConfirmation) -> Self {
        let mut view = Self::new(
            &c.connection_name,
            c.production,
            &c.statements,
            c.flagged.iter().map(|f| (f.index, f.class, f.reason)),
        );
        view.imported = c.imported;
        view
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
/// Shown by the SQL editor tab on `RunEvent::NeedsConfirmation`
/// (`connect_flow::open_statement_confirmation`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatementConfirmView {
    /// Connection name.
    pub connection_name: String,
    /// Typed production.
    pub production: bool,
    /// The statement exactly as it will run, bounded for display: the whole text up to
    /// [`STATEMENT_PREVIEW_MAX_BYTES`], its head and its tail beyond.
    pub statement: StatementPreview,
    /// `destructive: this connection type confirms…`.
    pub reason: String,
    /// Number of bind parameters.
    pub params: usize,
    /// Invisible bidirectional control characters in the editor's script when the run was asked
    /// (the text may run differently from how it reads).
    pub bidi_controls: usize,
}

impl StatementConfirmView {
    /// From the core's request, with the editor's count of bidirectional controls.
    #[must_use]
    pub fn new(r: &ConfirmationRequest, bidi_controls: usize) -> Self {
        Self {
            connection_name: r.connection_name.clone(),
            production: r.production,
            statement: StatementPreview::of(&r.statement),
            reason: format!("{}: {}", class_text(r.class), reason_text(r.reason)),
            params: r.params,
            bidi_controls,
        }
    }

    /// The warning about bidirectional controls, when there are any.
    #[must_use]
    pub fn bidi_warning(&self) -> Option<String> {
        (self.bidi_controls > 0).then(|| {
            format!(
                "Warning: the script holds {} invisible bidirectional control character(s): the statement may run differently from how it reads. Review it with \"Select bidirectional control characters\" (Alt+F8) before running.",
                self.bidi_controls
            )
        })
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
    fn a_short_statement_is_shown_whole() {
        let p = StatementPreview::of("DELETE FROM t");
        assert_eq!(p.head.as_ref(), "DELETE FROM t");
        assert_eq!(p.hidden_marker(), None);
        assert!(p.tail.is_empty());
        assert_eq!(p.size_text(), "1 line · 13 bytes");
        assert!(!p.needs_last_line());
        let p = StatementPreview::of("SELECT 1\nFROM t\n\n");
        assert_eq!(p.lines, 4);
        assert_eq!(p.last_line.as_ref(), "FROM t");
    }

    #[test]
    fn a_huge_statement_shows_its_head_its_tail_and_what_is_hidden() {
        // A long harmless head (with multi-byte characters around the cut points) and a
        // destructive tail.
        let mut text = String::from("SELECT 'é");
        while text.len() < 3 * 1024 * 1024 {
            text.push_str("ééé harmless text, ");
        }
        text.push_str("';\nDELETE FROM accounts");
        let p = StatementPreview::of(&text);
        assert!(p.head.len() <= STATEMENT_PREVIEW_PART_BYTES);
        assert!(p.tail.len() <= STATEMENT_PREVIEW_PART_BYTES + 1);
        assert!(
            p.tail.ends_with("DELETE FROM accounts"),
            "the end is visible"
        );
        assert_eq!(p.head.len() + p.hidden_bytes + p.tail.len(), text.len());
        let marker = p.hidden_marker().unwrap();
        assert_eq!(marker, format!("… {} bytes not shown …", p.hidden_bytes));
        assert_eq!(p.size_text(), format!("2 lines · {} bytes", text.len()));
        assert!(p.needs_last_line());
        assert_eq!(p.last_line.as_ref(), "DELETE FROM accounts");
        // One huge line: its end is still repeated under the box.
        let one = format!("{}DROP TABLE t", "x".repeat(100_000));
        let p = StatementPreview::of(&one);
        assert!(p.last_line.starts_with('…') && p.last_line.ends_with("DROP TABLE t"));
        assert_eq!(p.last_line.chars().count(), STATEMENT_LAST_LINE_CHARS + 1);
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
    fn imported_init_sql_has_its_own_reason_and_notice() {
        let text = reason_text(ConfirmReason::Imported);
        assert!(text.contains("imported and not confirmed yet"), "{text}");
        assert!(!text.contains("connection type"), "{text}");
        let statements = vec!["SET search_path TO app".to_owned()];
        let v = InitSqlView::new(
            "Imported",
            false,
            &statements,
            [(0, StatementClass::Session, ConfirmReason::Imported)],
        );
        assert!(!v.imported, "set from InitSqlConfirmation::imported only");
        assert_eq!(
            v.rows[0].flag.as_deref(),
            Some(
                "session setting: imported and not confirmed yet; it runs at every connect once you confirm"
            )
        );
        assert!(IMPORTED_INIT_SQL_NOTICE.contains("have not been confirmed yet"));
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
