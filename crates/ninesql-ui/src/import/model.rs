//! View model of the DBeaver import preview and outcome (P2-B, F-CON-14): what the preview
//! screen shows, built from the core's `ImportPreview` without any I/O, and the user's choices
//! turned into `ImportChoices`.
//!
//! I1: nothing here holds or formats a secret value. Secrets appear as counts and credential
//! fields ("Password (database)"); the import report entries are the core's messages, which
//! never quote a value from the DBeaver files.

use std::collections::BTreeSet;

use ninesql_core::dbeaver::mapping::CredentialsStatus;
use ninesql_core::dbeaver::{ImportIssue, InstallKind, IssueKind, Severity as IssueSeverity};
use ninesql_core::secrets::{SecretKind, StoreKind};
use ninesql_core::{
    ConnectionPlan, ConnectionType, ConnectionTypeId, ConnectionTypes, Connections, DisabledKind,
    Endpoint, ImportChoices, ImportOutcome, ImportPreview, ImportStatus, PlanAction, SafetyPolicy,
    SecretChangeKind, SecretSlot, TlsMode, TypeAction, UpdateDiff,
};

use crate::connections::engines::tls_label;

/// Shown next to imported init SQL (ADR-0022 §1).
pub const INIT_SQL_NOTE: &str = "will ask for confirmation on first connect";

/// Shown with the "accept the imported TLS settings" toggle (I5).
/// Shown before and after the box is ticked.
pub const TLS_ACCEPT_WARNING: &str = "The imported TLS settings are weaker than, or differ from, the ones saved in NineSQL. Accept them only if you know the server needs them: a weaker mode lets a man in the middle read or change the traffic, and a different CA file lets whoever holds that CA impersonate the server.";

/// Shown when a route reason must be confirmed before enabling.
pub const ROUTE_CONFIRMATION: &str =
    "I set up the route again in NineSQL (SSH tunnel or network) and checked it";

/// The advice of ADR-0006 §4, shown after an import that moved saved passwords.
pub const DELETE_DBEAVER_PASSWORDS_ADVICE: &str = "DBeaver still keeps the passwords it saved, in a file that is only obfuscated. Once you have switched to NineSQL, consider deleting them in DBeaver (edit each connection and clear \"Save password\"). NineSQL never changes DBeaver files. The import report says so too.";

/// Label of a credential field.
#[must_use]
pub fn slot_label(slot: SecretSlot) -> String {
    match slot {
        SecretSlot::Auth => "database".to_owned(),
        SecretSlot::SshServer => "SSH server".to_owned(),
        SecretSlot::SshJump(i) => format!("SSH jump host {}", i + 1),
        SecretSlot::TlsClientKey => "TLS client key".to_owned(),
    }
}

/// Label of a kind of secret.
#[must_use]
pub fn kind_label(kind: SecretKind) -> &'static str {
    match kind {
        SecretKind::Password => "Password",
        SecretKind::SshPassword => "SSH password",
        SecretKind::SshKeyPassphrase => "SSH key passphrase",
        SecretKind::Token => "Token",
        SecretKind::ClientKeyPassphrase => "TLS client key passphrase",
        SecretKind::ProxyPassword => "Proxy password",
        #[allow(unreachable_patterns)]
        _ => "Secret",
    }
}

/// "Password (database)": a secret to move, never its value.
#[must_use]
pub fn secret_label(slot: SecretSlot, kind: SecretKind) -> String {
    format!("{} ({})", kind_label(kind), slot_label(slot))
}

/// Where the imported secrets go.
#[must_use]
pub fn store_label(store: Option<StoreKind>) -> &'static str {
    match store {
        Some(StoreKind::Keyring) => "the desktop keyring (Secret Service)",
        Some(StoreKind::Vault) => "the encrypted vault",
        None => "nowhere: no keyring or vault is available",
    }
}

/// Short label of a disabled reason kind.
#[must_use]
pub fn disabled_kind_label(kind: DisabledKind) -> &'static str {
    match kind {
        DisabledKind::NoDriver => "no driver",
        DisabledKind::Route => "route not reproduced",
        DisabledKind::Invalid => "settings refused by the driver",
        #[allow(unreachable_patterns)]
        _ => "disabled",
    }
}

/// How DBeaver was installed.
#[must_use]
pub fn install_label(kind: InstallKind) -> &'static str {
    match kind {
        InstallKind::Native => "native",
        InstallKind::Flatpak => "Flatpak",
        InstallKind::Snap => "Snap",
        InstallKind::Custom => "chosen folder",
    }
}

fn endpoint_text(e: &Endpoint) -> String {
    match e {
        Endpoint::Tcp {
            host,
            port,
            database,
        } => {
            let mut s = host.clone();
            if let Some(p) = port {
                s.push_str(&format!(":{p}"));
            }
            if let Some(db) = database {
                s.push_str(&format!("/{db}"));
            }
            s
        }
        Endpoint::UnixSocket { path, database } => match database {
            Some(db) => format!("{} /{db}", path.display()),
            None => path.display().to_string(),
        },
        Endpoint::File { path } => path.display().to_string(),
    }
}

fn tls_text((mode, ca): &(TlsMode, Option<std::path::PathBuf>)) -> String {
    match ca {
        Some(ca) => format!("{} (CA file {})", tls_label(*mode), ca.display()),
        None => tls_label(*mode).to_owned(),
    }
}

/// One report entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueRow {
    /// `Info`, `Warning` or `Error`.
    pub severity: IssueSeverity,
    /// DBeaver key concerned, if any.
    pub key: Option<String>,
    /// The core's message (no value from the file).
    pub message: String,
    /// What kind of entry it is (`Truncated` and `Duplicate` are shown as source issues).
    pub kind: IssueKind,
}

impl IssueRow {
    fn of(i: &ImportIssue) -> Self {
        Self {
            severity: i.severity,
            key: i.key.clone(),
            message: i.message.clone(),
            kind: i.kind,
        }
    }

    /// A source-level entry: a limit reached (the rest was not read) or a folder read once.
    #[must_use]
    pub fn is_source_issue(&self) -> bool {
        matches!(self.kind, IssueKind::Truncated | IssueKind::Duplicate)
    }

    /// One line for the screen.
    #[must_use]
    pub fn text(&self) -> String {
        let level = match self.severity {
            IssueSeverity::Info => "Note",
            IssueSeverity::Warning => "Warning",
            IssueSeverity::Error => "Error",
        };
        match &self.key {
            Some(k) => format!("{level} ({k}): {}", self.message),
            None => format!("{level}: {}", self.message),
        }
    }
}

/// What applying does with one row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    /// New connection, enabled.
    Create,
    /// Updates a connection imported before (same DBeaver id, project and workspace).
    Update,
    /// Imported disabled (see `reason`).
    Disabled,
    /// Not imported (see `reason`).
    NotImported,
}

impl RowKind {
    /// Badge text.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Create => "new",
            Self::Update => "update",
            Self::Disabled => "imported disabled",
            Self::NotImported => "not imported",
        }
    }
}

/// TLS difference of an update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsView {
    /// Saved in NineSQL.
    pub saved: String,
    /// From DBeaver.
    pub imported: String,
    /// The saved settings are kept unless the user accepts the imported ones.
    pub kept_saved: bool,
}

/// Field-level differences of an update.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiffView {
    /// "Endpoint: a → b" lines (no value of driver properties, keys only).
    pub changes: Vec<String>,
    /// TLS difference.
    pub tls: Option<TlsView>,
    /// Saved secrets dropped (asked at connect afterwards).
    pub secrets_dropped: Vec<String>,
    /// Saved secrets replaced by DBeaver's.
    pub secrets_replaced: Vec<String>,
    /// Saved secrets kept.
    pub secrets_kept: Vec<String>,
}

impl DiffView {
    fn of(diff: &UpdateDiff, types: &ConnectionTypes) -> Self {
        let mut changes = Vec::new();
        if let Some(c) = &diff.engine {
            changes.push(format!("Engine: {} → {}", c.saved, c.imported));
        }
        if let Some(c) = &diff.endpoint {
            changes.push(format!(
                "Endpoint: {} → {}",
                endpoint_text(&c.saved),
                endpoint_text(&c.imported)
            ));
        }
        if let Some(c) = &diff.username {
            let show = |u: &Option<String>| u.clone().unwrap_or_else(|| "(none)".to_owned());
            changes.push(format!(
                "User name: {} → {}",
                show(&c.saved),
                show(&c.imported)
            ));
        }
        if let Some(c) = &diff.ssh {
            let show = |h: &Vec<String>| {
                if h.is_empty() {
                    "no tunnel".to_owned()
                } else {
                    h.join(" → ")
                }
            };
            changes.push(format!(
                "SSH route: {} ⇒ {}",
                show(&c.saved),
                show(&c.imported)
            ));
        }
        if let Some((c, kept)) = &diff.connection_type {
            let name = |id: &ConnectionTypeId| types.resolve(id).name;
            if *kept {
                changes.push(format!(
                    "Connection type: {} kept (DBeaver's {} is less strict)",
                    name(&c.saved),
                    name(&c.imported)
                ));
            } else {
                changes.push(format!(
                    "Connection type: {} → {}",
                    name(&c.saved),
                    name(&c.imported)
                ));
            }
        }
        if let Some(c) = &diff.properties {
            let keys: BTreeSet<&String> = c
                .saved
                .iter()
                .chain(c.imported.iter())
                .filter(|(k, _)| c.saved.get(*k) != c.imported.get(*k))
                .map(|(k, _)| k)
                .collect();
            let keys: Vec<&str> = keys.into_iter().map(String::as_str).collect();
            changes.push(format!("Driver properties changed: {}", keys.join(", ")));
        }
        if let Some(c) = &diff.init_sql {
            changes.push(format!(
                "Init SQL: {} → {} statement(s), {INIT_SQL_NOTE}",
                c.saved.len(),
                c.imported.len()
            ));
        }
        let mut out = Self {
            changes,
            tls: diff.tls.as_ref().map(|t| TlsView {
                saved: tls_text(&t.saved),
                imported: tls_text(&t.imported),
                kept_saved: t.kept_saved,
            }),
            ..Self::default()
        };
        for s in &diff.secrets {
            let label = slot_label(s.slot);
            match s.change {
                SecretChangeKind::Dropped => out.secrets_dropped.push(label),
                SecretChangeKind::Replaced => out.secrets_replaced.push(label),
                SecretChangeKind::Kept => out.secrets_kept.push(label),
            }
        }
        out
    }
}

/// One DBeaver connection in the preview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionRow {
    /// DBeaver id (key of the choices).
    pub dbeaver_id: String,
    /// Name.
    pub name: String,
    /// What applying does.
    pub kind: RowKind,
    /// It updates a connection imported before (also for a disabled row).
    pub updates: bool,
    /// Engine id.
    pub engine: Option<String>,
    /// Connection type name.
    pub type_name: Option<String>,
    /// Folder path.
    pub folder: Option<String>,
    /// Why it is disabled or not imported.
    pub reason: Option<String>,
    /// The disabled reason needs an "I fixed the route" confirmation to enable.
    pub route_reason: bool,
    /// Secrets to move, as labels.
    pub secrets: Vec<String>,
    /// Number of init SQL statements ([`INIT_SQL_NOTE`] when non-zero).
    pub init_sql: usize,
    /// Differences of an update.
    pub diff: Option<DiffView>,
    /// TLS settings of a new connection over the network (`Create` rows only; an update shows
    /// its TLS change in [`DiffView::tls`]).
    pub create_tls: Option<RowTls>,
    /// Report entries.
    pub issues: Vec<IssueRow>,
    /// The user can leave it out.
    pub skippable: bool,
    /// "Test" is offered (imported enabled).
    pub testable: bool,
}

impl ConnectionRow {
    fn of(plan: &ConnectionPlan, types: &ConnectionTypes) -> Self {
        let c = plan.connection.as_ref();
        let updates = matches!(plan.action, PlanAction::Update { .. });
        let kind = match (plan.status, &plan.action) {
            (ImportStatus::Failed, _) | (_, PlanAction::NotImported { .. }) => RowKind::NotImported,
            (ImportStatus::ImportedDisabled, _) => RowKind::Disabled,
            (_, PlanAction::Update { .. }) => RowKind::Update,
            _ => RowKind::Create,
        };
        let reason = match &plan.action {
            PlanAction::NotImported { reason } => Some(reason.clone()),
            _ => c.and_then(|c| {
                c.disabled_reason
                    .as_ref()
                    .map(|r| format!("{}: {}", disabled_kind_label(r.kind), r.message))
            }),
        };
        let route_reason = c
            .and_then(|c| c.disabled_reason.as_ref())
            .is_some_and(|r| r.kind == DisabledKind::Route);
        let diff = match &plan.action {
            PlanAction::Update { diff, .. } => Some(DiffView::of(diff, types)),
            _ => None,
        };
        let create_tls = c
            .filter(|_| kind == RowKind::Create)
            .filter(|c| c.endpoint.is_network())
            .map(|c| RowTls {
                text: tls_text(&(c.tls.mode, c.tls.ca_file.clone())),
                weak: c.tls.mode != TlsMode::VerifyFull,
            });
        Self {
            dbeaver_id: plan.dbeaver_id.clone(),
            name: plan.name.clone(),
            kind,
            updates,
            engine: c.map(|c| c.engine.to_string()),
            type_name: c.map(|c| types.resolve(&c.connection_type).name),
            folder: c
                .filter(|c| !c.folder.is_empty())
                .map(|c| c.folder.join("/")),
            reason,
            route_reason,
            secrets: plan
                .secrets
                .iter()
                .map(|s| secret_label(s.slot, s.kind))
                .collect(),
            init_sql: c.map_or(0, |c| c.init_sql.len()),
            diff,
            create_tls,
            issues: plan.issues.iter().map(IssueRow::of).collect(),
            skippable: kind != RowKind::NotImported,
            testable: c.is_some_and(|c| c.enabled) && kind != RowKind::NotImported,
        }
    }

    /// The update keeps a saved TLS setting unless the user accepts the imported one.
    #[must_use]
    pub fn tls_kept(&self) -> bool {
        self.diff
            .as_ref()
            .and_then(|d| d.tls.as_ref())
            .is_some_and(|t| t.kept_saved)
    }
}

/// TLS settings of a new connection, as shown on its row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowTls {
    /// Mode, and CA file if any.
    pub text: String,
    /// Weaker than verify-full (shown in the warning colour, I5).
    pub weak: bool,
}

/// One DBeaver project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectView {
    /// Name.
    pub name: String,
    /// Folder.
    pub path: String,
    /// What happened to DBeaver's saved passwords file.
    pub credentials: String,
    /// Empty folders, listed but not created.
    pub unused_folders: Vec<String>,
    /// Connections.
    pub connections: Vec<ConnectionRow>,
    /// Project-level report entries.
    pub issues: Vec<IssueRow>,
}

/// What happens to a connection type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeRowKind {
    /// New type.
    Create,
    /// An existing type is made stricter.
    Tighten,
    /// Unchanged.
    Keep,
}

/// One connection type in the preview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeRow {
    /// NineSQL id.
    pub id: String,
    /// Name (NineSQL's for an existing type).
    pub name: String,
    /// What happens.
    pub kind: TypeRowKind,
    /// A built-in type (Development, Test, Production).
    pub builtin: bool,
    /// The rules it gains (tighten) or has (create).
    pub changes: Vec<String>,
    /// Saved connections of this type, whose rules change with it.
    pub existing_connections: usize,
}

impl TypeRow {
    /// A tightened type that saved connections already use: their behaviour changes.
    #[must_use]
    pub fn changes_existing(&self) -> bool {
        self.kind == TypeRowKind::Tighten && self.existing_connections > 0
    }

    /// One line for the screen.
    #[must_use]
    pub fn text(&self) -> String {
        let rules = if self.changes.is_empty() {
            String::new()
        } else {
            format!(": {}", self.changes.join(", "))
        };
        match self.kind {
            TypeRowKind::Create => format!("New type \"{}\"{rules}", self.name),
            TypeRowKind::Keep => format!("\"{}\" unchanged", self.name),
            TypeRowKind::Tighten => {
                let builtin = if self.builtin { "built-in " } else { "" };
                let mut s = format!(
                    "The {builtin}type \"{}\" becomes stricter{rules}",
                    self.name
                );
                if self.existing_connections > 0 {
                    s.push_str(&format!(
                        ". This changes {} saved connection(s) of this type",
                        self.existing_connections
                    ));
                }
                s
            }
        }
    }
}

/// The rules `after` adds to `before`.
#[must_use]
pub fn stricter_rules(before: &SafetyPolicy, after: &SafetyPolicy) -> Vec<String> {
    let mut out = Vec::new();
    if after.production && !before.production {
        out.push("production rules (colored frame, confirmations)".to_owned());
    }
    if after.confirm_destructive && !before.confirm_destructive {
        out.push("asks before destructive statements".to_owned());
    }
    if after.confirm_every_write && !before.confirm_every_write {
        out.push("asks before every write".to_owned());
    }
    if after.read_only && !before.read_only {
        out.push("read-only by default".to_owned());
    }
    if !after.auto_commit && before.auto_commit {
        out.push("manual commit by default".to_owned());
    }
    if !after.log_query_text && before.log_query_text {
        out.push("query text never logged".to_owned());
    }
    out
}

fn is_builtin(id: &ConnectionTypeId) -> bool {
    *id == ConnectionTypeId::dev()
        || *id == ConnectionTypeId::test()
        || *id == ConnectionTypeId::prod()
}

/// Everything the preview screen shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewView {
    /// Workspaces read ("path (native, DBeaver 25.3.5)").
    pub sources: Vec<String>,
    /// Secrets the import would move.
    pub secret_count: usize,
    /// Where they go by default.
    pub default_store: Option<StoreKind>,
    /// Connection types.
    pub types: Vec<TypeRow>,
    /// Projects.
    pub projects: Vec<ProjectView>,
    /// Report entries not tied to a project.
    pub issues: Vec<IssueRow>,
    /// A limit was reached while reading the source (`ImportPreview::truncated`): the preview
    /// is incomplete, shown prominently.
    pub truncated: bool,
}

impl PreviewView {
    /// Source issues of the whole preview (limits reached, folders read once), the source-level
    /// ones first, then those of each project.
    pub fn source_issues(&self) -> impl Iterator<Item = &IssueRow> {
        self.issues
            .iter()
            .chain(self.projects.iter().flat_map(|p| p.issues.iter()))
            .filter(|i| i.is_source_issue())
    }

    /// The view of `preview` against the hub's current types and connections.
    #[must_use]
    pub fn build(preview: &ImportPreview, types: &ConnectionTypes, saved: &Connections) -> Self {
        let after = preview.types_after(types);
        let type_rows = preview
            .connection_types
            .iter()
            .map(|t| {
                let existing_connections =
                    saved.iter().filter(|c| c.connection_type == t.id).count();
                let (kind, changes) = match &t.action {
                    TypeAction::Create(new) => (
                        TypeRowKind::Create,
                        stricter_rules(&ConnectionType::development().policy(), &new.policy()),
                    ),
                    TypeAction::Tighten(new) => (
                        TypeRowKind::Tighten,
                        stricter_rules(&types.resolve(&t.id).policy(), &new.policy()),
                    ),
                    TypeAction::Keep => (TypeRowKind::Keep, Vec::new()),
                };
                let name = match &t.action {
                    TypeAction::Create(_) => t.name.clone(),
                    _ => types.resolve(&t.id).name,
                };
                TypeRow {
                    id: t.id.to_string(),
                    name,
                    kind,
                    builtin: is_builtin(&t.id),
                    changes,
                    existing_connections,
                }
            })
            .collect();
        Self {
            sources: preview
                .workspaces
                .iter()
                .map(|w| match &w.product_version {
                    Some(v) => format!(
                        "{} ({}, DBeaver {v})",
                        w.path.display(),
                        install_label(w.kind)
                    ),
                    None => format!("{} ({})", w.path.display(), install_label(w.kind)),
                })
                .collect(),
            secret_count: preview.secret_count(),
            default_store: preview.default_store,
            types: type_rows,
            projects: preview
                .projects
                .iter()
                .map(|p| ProjectView {
                    name: p.name.clone(),
                    path: p.path.display().to_string(),
                    credentials: match &p.credentials {
                        CredentialsStatus::NotFound => "No saved passwords file.".to_owned(),
                        CredentialsStatus::Decrypted { passwords } => {
                            format!("Saved passwords file read: {passwords} saved password(s).")
                        }
                        CredentialsStatus::ReadFailed => {
                            "The saved passwords file could not be read: connections are imported without passwords.".to_owned()
                        }
                        CredentialsStatus::Unreadable(_) => {
                            "The saved passwords file is protected or not in DBeaver Community's format: connections are imported without passwords.".to_owned()
                        }
                    },
                    unused_folders: p
                        .folders
                        .iter()
                        .filter(|f| !f.used)
                        .map(|f| f.path.join("/"))
                        .collect(),
                    connections: p
                        .connections
                        .iter()
                        .map(|c| ConnectionRow::of(c, &after))
                        .collect(),
                    issues: p.issues.iter().map(IssueRow::of).collect(),
                })
                .collect(),
            issues: preview.issues.iter().map(IssueRow::of).collect(),
            truncated: preview.truncated(),
        }
    }

    /// Every connection row.
    pub fn rows(&self) -> impl Iterator<Item = &ConnectionRow> {
        self.projects.iter().flat_map(|p| p.connections.iter())
    }

    /// A row by DBeaver id.
    #[must_use]
    pub fn row(&self, dbeaver_id: &str) -> Option<&ConnectionRow> {
        self.rows().find(|r| r.dbeaver_id == dbeaver_id)
    }

    /// Tightened types that change saved connections.
    pub fn tightened_in_use(&self) -> impl Iterator<Item = &TypeRow> {
        self.types.iter().filter(|t| t.changes_existing())
    }

    /// Every text of the view, for tests (I1: no secret value may appear in it).
    #[must_use]
    pub fn all_text(&self) -> String {
        let mut out = self.sources.join("\n");
        for t in &self.types {
            out.push('\n');
            out.push_str(&t.text());
        }
        for i in &self.issues {
            out.push('\n');
            out.push_str(&i.text());
        }
        for p in &self.projects {
            out.push_str(&format!("\n{} {} {}", p.name, p.path, p.credentials));
            for i in &p.issues {
                out.push('\n');
                out.push_str(&i.text());
            }
            for r in &p.connections {
                out.push_str(&format!("\n{r:?}"));
            }
        }
        out
    }
}

/// What the user chose on the preview screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    /// DBeaver ids left out.
    pub skip: BTreeSet<String>,
    /// DBeaver ids whose imported TLS settings are accepted (I5 warning shown).
    pub accept_tls: BTreeSet<String>,
    /// Move the saved passwords to the store (`false`: asked at connect).
    pub import_secrets: bool,
    /// Update connections imported before.
    pub update_existing: bool,
}

impl Default for Selection {
    fn default() -> Self {
        Self {
            skip: BTreeSet::new(),
            accept_tls: BTreeSet::new(),
            import_secrets: true,
            update_existing: true,
        }
    }
}

impl Selection {
    /// The core's choices: `store` is the preview's default store when secrets are imported.
    /// Only ids of rows that offer the choice are passed.
    #[must_use]
    pub fn choices(&self, view: &PreviewView) -> ImportChoices {
        let store = if self.import_secrets {
            view.default_store
        } else {
            None
        };
        let mut choices = ImportChoices::all(store);
        choices.update_existing = self.update_existing;
        choices.skip = self
            .skip
            .iter()
            .filter(|id| view.row(id).is_some_and(|r| r.skippable))
            .cloned()
            .collect();
        choices.accept_tls_changes = self
            .accept_tls
            .iter()
            .filter(|id| view.row(id).is_some_and(ConnectionRow::tls_kept))
            .cloned()
            .collect();
        choices
    }

    /// Rows that will be written with these choices.
    #[must_use]
    pub fn selected_count(&self, view: &PreviewView) -> usize {
        view.rows()
            .filter(|r| r.skippable && !self.skip.contains(&r.dbeaver_id))
            .filter(|r| self.update_existing || !r.updates)
            .count()
    }
}

/// What the outcome screen shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutcomeView {
    /// "3 created, 1 updated…" lines.
    pub lines: Vec<String>,
    /// The report saved in the project; `None` when it could not be written.
    pub report_file: Option<String>,
    /// Why the report could not be saved ("report not saved: <reason>"); the import itself
    /// was applied. Never holds a secret (a storage error).
    pub report_error: Option<String>,
    /// The ADR-0006 §4 advice, when saved passwords were moved or found.
    pub advice: Option<&'static str>,
}

impl OutcomeView {
    /// The view of `outcome`; `had_saved_passwords` comes from the preview (some connection had
    /// a saved password in DBeaver).
    #[must_use]
    pub fn build(outcome: &ImportOutcome, had_saved_passwords: bool) -> Self {
        let mut lines = vec![format!(
            "{} connection(s) created, {} updated ({} of them disabled).",
            outcome.created, outcome.updated, outcome.disabled
        )];
        if outcome.skipped > 0 {
            lines.push(format!("{} left out as you chose.", outcome.skipped));
        }
        if outcome.failed > 0 {
            lines.push(format!("{} not imported (see the report).", outcome.failed));
        }
        lines.push(format!(
            "{} saved password(s) moved to the secret store.",
            outcome.secrets_moved
        ));
        if outcome.types_changed > 0 {
            lines.push(format!(
                "{} connection type(s) created or made stricter.",
                outcome.types_changed
            ));
        }
        Self {
            lines,
            report_file: outcome
                .report_file
                .as_ref()
                .map(|p| p.display().to_string()),
            report_error: outcome.report_error.clone(),
            advice: (had_saved_passwords || outcome.secrets_moved > 0)
                .then_some(DELETE_DBEAVER_PASSWORDS_ADVICE),
        }
    }
}
