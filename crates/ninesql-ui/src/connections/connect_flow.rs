//! The connect flow of one window (F-CON-01 and the P1-C follow-ups): calls
//! `SessionManager::open` on the driver runtime, awaits the `Reply` on GPUI's executor (I6) and
//! answers every `OpenOutcome`:
//! - `NeedsSecrets`: a dialog with one masked field per prompt, then open again with the
//!   answers (kept only for that request, zeroized when dropped);
//! - `HostKeyNeedsConfirmation`: hop, key type, SHA-256 fingerprint and `other_key_types`;
//!   "Trust" sends the `HostKeyToken` to `trust_host_key`, then opens again (the prompted secrets
//!   are asked again: nobody keeps them while the user decides);
//! - `InitSqlNeedsConfirmation`: every init statement, flagged ones marked; "Run" opens again
//!   with the `InitSqlToken`;
//! - `NeedsVaultUnlock`: the vault's master-password prompt ([`super::vault`]), then open
//!   again (the prompted secrets are asked again);
//! - `Opened`: the session is recorded in the hub (or closed at once for "Test connection"),
//!   and its security warnings are shown.

use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::InputState;
use gpui_kit::component::{ActiveTheme as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use ninesql_core::secrets::{SecretError, SecretString, StoreKind};
use ninesql_core::{
    Connection, ConnectionId, CoreError, HostKeyToken, InitSqlToken, OpenOutcome, OpenRequest,
    SecretPrompt, SecretSlot,
};

use super::flow::{
    HostKeyView, IMPORTED_INIT_SQL_NOTICE, InitSqlView, SecretPromptView, StatementConfirmView,
    session_warning_text,
};
use super::hub::ConnectionHub;
use super::secret_input::{self, read_secret, secret_field};
use super::vault::{VaultMode, VaultPromptEvent, open_vault_prompt};
use super::visuals::{hsla, text_on};

/// Why a connection is being opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    /// Open a session and keep it.
    Open,
    /// "Test connection": open, report, close.
    Test,
}

/// How a run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlowResult {
    /// A session is open.
    Opened {
        /// The connection.
        connection: ConnectionId,
        /// Security warnings of the session, as shown.
        warnings: Vec<String>,
    },
    /// "Test connection" succeeded.
    TestOk {
        /// Server version, when reported.
        server_version: Option<String>,
        /// Security warnings of the session, as shown.
        warnings: Vec<String>,
    },
    /// It failed; the message never contains a secret.
    Failed(String),
    /// The user cancelled a prompt.
    Cancelled,
}

/// The prompt the flow is waiting on, for the dialogs and for tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingPrompt {
    /// Nothing.
    None,
    /// The connect is running on the driver runtime.
    Running,
    /// Secrets to type.
    Secrets(SecretPromptView),
    /// An SSH host key to confirm.
    HostKey(HostKeyView),
    /// Init SQL to confirm.
    InitSql(InitSqlView),
    /// The vault's master password.
    VaultUnlock,
}

struct Run {
    purpose: Purpose,
    connection: Connection,
    /// Connection types for the policy of this run instead of the hub's (the test of an import
    /// preview runs under the types the import would save, `ImportPreview::types_after`).
    types: Option<ninesql_core::ConnectionTypes>,
    prompted: Vec<(SecretSlot, SecretString)>,
    init_token: Option<InitSqlToken>,
}

enum Waiting {
    Secrets {
        prompts: Vec<SecretPrompt>,
        inputs: Vec<Entity<InputState>>,
    },
    HostKey(HostKeyToken),
    InitSql(InitSqlToken),
    Vault(Entity<super::vault::VaultPrompt>),
}

/// Emitted when a run ends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlowFinished {
    /// Why the connection was opened.
    pub purpose: Purpose,
    /// How it ended.
    pub result: FlowResult,
}

/// The connect flow of one window.
pub struct ConnectFlow {
    hub: Entity<ConnectionHub>,
    run: Option<Run>,
    waiting: Option<Waiting>,
    prompt: PendingPrompt,
    last: Option<FlowFinished>,
    task: Option<Task<()>>,
}

impl EventEmitter<FlowFinished> for ConnectFlow {}

impl ConnectFlow {
    /// A flow on `hub`.
    pub fn new(hub: Entity<ConnectionHub>) -> Self {
        Self {
            hub,
            run: None,
            waiting: None,
            prompt: PendingPrompt::None,
            last: None,
            task: None,
        }
    }

    /// The prompt in progress.
    #[must_use]
    pub fn prompt(&self) -> &PendingPrompt {
        &self.prompt
    }

    /// How the last run ended.
    #[must_use]
    pub fn last_result(&self) -> Option<&FlowFinished> {
        self.last.as_ref()
    }

    /// Whether a run is in progress (running or waiting on the user).
    #[must_use]
    pub fn is_busy(&self) -> bool {
        self.run.is_some()
    }

    /// The vault prompt in progress, if any (tests type into it).
    #[must_use]
    pub fn vault_prompt(&self) -> Option<Entity<super::vault::VaultPrompt>> {
        match &self.waiting {
            Some(Waiting::Vault(p)) => Some(p.clone()),
            _ => None,
        }
    }

    /// The masked inputs of a secret prompt (tests type into them).
    #[must_use]
    pub fn secret_inputs(&self) -> Vec<Entity<InputState>> {
        match &self.waiting {
            Some(Waiting::Secrets { inputs, .. }) => inputs.clone(),
            _ => Vec::new(),
        }
    }

    /// Starts opening `connection`. `typed` are secrets typed in the connection dialog for a
    /// connection not saved yet ("Test connection").
    pub fn start(
        &mut self,
        connection: Connection,
        purpose: Purpose,
        typed: Vec<(SecretSlot, SecretString)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.start_with_types(connection, purpose, typed, None, window, cx);
    }

    /// [`Self::start`] under `types` instead of the hub's connection types (`None`: the hub's).
    /// The DBeaver import preview tests a connection not saved yet under the types the import
    /// would save (`ImportPreview::types_after`), so a type the import tightens applies.
    pub fn start_with_types(
        &mut self,
        connection: Connection,
        purpose: Purpose,
        typed: Vec<(SecretSlot, SecretString)>,
        types: Option<ninesql_core::ConnectionTypes>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.waiting = None;
        self.run = Some(Run {
            purpose,
            connection,
            types,
            prompted: typed,
            init_token: None,
        });
        self.dispatch(window, cx);
    }

    fn dispatch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(run) = self.run.as_mut() else {
            return;
        };
        let hub = self.hub.read(cx);
        let Some(manager) = hub.manager().cloned() else {
            self.finish_in(
                FlowResult::Failed(
                    "the driver runtime is not running: connections cannot be opened".to_owned(),
                ),
                window,
                cx,
            );
            return;
        };
        let mut request = OpenRequest::new(
            run.connection.clone(),
            run.types.as_ref().unwrap_or_else(|| hub.types()),
        );
        // Answers are moved into this request only (zeroized when it is dropped).
        for (slot, secret) in run.prompted.drain(..) {
            request = request.with_secret(slot, secret);
        }
        if let Some(token) = run.init_token {
            request = request.with_init_sql_confirmation(token);
        }
        let reply = manager.open(request);
        self.prompt = PendingPrompt::Running;
        cx.notify();
        self.task = Some(cx.spawn_in(window, async move |this, cx| {
            let outcome = reply.await;
            let _ = this.update_in(cx, |flow, window, cx| flow.on_outcome(outcome, window, cx));
        }));
    }

    /// Applies an open outcome (public for tests that drive the flow step by step).
    pub fn on_outcome(
        &mut self,
        outcome: Option<Result<OpenOutcome, CoreError>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(run) = self.run.as_ref() else {
            return;
        };
        let purpose = run.purpose;
        match outcome {
            None => self.finish_in(
                FlowResult::Failed("the connection attempt stopped".to_owned()),
                window,
                cx,
            ),
            Some(Err(e)) => self.finish_in(FlowResult::Failed(error_text(&e)), window, cx),
            Some(Ok(OpenOutcome::Opened(handle))) => {
                let info = handle.info();
                let mut warnings: Vec<String> = info
                    .warnings
                    .iter()
                    .map(|w| session_warning_text(w, info.tunnel.as_ref()))
                    .collect();
                if info.inside_dbeaver_workspace {
                    warnings.push(DBEAVER_READ_ONLY.to_owned());
                }
                let result = match purpose {
                    Purpose::Test => {
                        let server_version = info.server_version.clone();
                        drop(handle.close());
                        FlowResult::TestOk {
                            server_version,
                            warnings,
                        }
                    }
                    Purpose::Open => {
                        let connection = info.connection;
                        self.hub
                            .update(cx, |hub, cx| hub.add_session(handle.clone(), cx));
                        FlowResult::Opened {
                            connection,
                            warnings,
                        }
                    }
                };
                self.finish_in(result, window, cx);
            }
            Some(Ok(OpenOutcome::NeedsSecrets(prompts))) => {
                let inputs: Vec<Entity<InputState>> = prompts
                    .iter()
                    .map(|p| secret_input::masked_input(window, cx, &p.label))
                    .collect();
                self.prompt = PendingPrompt::Secrets(SecretPromptView::new(&prompts));
                self.waiting = Some(Waiting::Secrets { prompts, inputs });
                open_prompt_dialog(cx.entity(), window, cx);
            }
            Some(Ok(OpenOutcome::HostKeyNeedsConfirmation(conf))) => {
                self.prompt = PendingPrompt::HostKey(HostKeyView::new(&conf.key));
                self.waiting = Some(Waiting::HostKey(conf.token));
                open_prompt_dialog(cx.entity(), window, cx);
            }
            Some(Ok(OpenOutcome::InitSqlNeedsConfirmation(conf))) => {
                self.prompt = PendingPrompt::InitSql(InitSqlView::from_confirmation(&conf));
                self.waiting = Some(Waiting::InitSql(conf.token));
                open_prompt_dialog(cx.entity(), window, cx);
            }
            Some(Ok(OpenOutcome::NeedsVaultUnlock { .. })) => {
                if self.hub.read(cx).core().is_none() {
                    self.finish_in(
                        FlowResult::Failed(
                            "The vault is locked and cannot be unlocked in this session."
                                .to_owned(),
                        ),
                        window,
                        cx,
                    );
                } else {
                    let prompt = open_vault_prompt(self.hub.clone(), VaultMode::Unlock, window, cx);
                    cx.subscribe_in(&prompt, window, |flow, _, event, window, cx| {
                        if !matches!(flow.waiting, Some(Waiting::Vault(_))) {
                            return;
                        }
                        match event {
                            VaultPromptEvent::Done => {
                                flow.waiting = None;
                                // Open again; secrets asked earlier are asked again.
                                flow.dispatch(window, cx);
                            }
                            VaultPromptEvent::Cancelled => {
                                flow.finish_in(FlowResult::Cancelled, window, cx);
                            }
                        }
                    })
                    .detach();
                    self.prompt = PendingPrompt::VaultUnlock;
                    self.waiting = Some(Waiting::Vault(prompt));
                }
            }
            Some(Ok(_)) => self.finish_in(
                FlowResult::Failed("this NineSQL build cannot handle the core's answer".to_owned()),
                window,
                cx,
            ),
        }
        cx.notify();
    }

    /// Answers a secret prompt with what was typed, then opens again.
    pub fn submit_secrets(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Waiting::Secrets { prompts, inputs }) = self.waiting.take() else {
            return;
        };
        let Some(run) = self.run.as_mut() else {
            for input in &inputs {
                secret_input::clear(input, window, cx);
            }
            return;
        };
        for (prompt, input) in prompts.iter().zip(&inputs) {
            // An empty answer is a valid (empty) password.
            let secret =
                read_secret(input, cx).unwrap_or_else(|| SecretString::from(String::new()));
            secret_input::clear(input, window, cx);
            run.prompted.push((prompt.slot, secret));
        }
        window.close_dialog(cx);
        self.dispatch(window, cx);
    }

    /// Trusts the host key shown (with the core's single-use token), then opens again.
    pub fn trust_host_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Waiting::HostKey(token)) = self.waiting.take() else {
            return;
        };
        window.close_dialog(cx);
        let Some(manager) = self.hub.read(cx).manager().cloned() else {
            return;
        };
        let reply = manager.trust_host_key(token);
        self.prompt = PendingPrompt::Running;
        cx.notify();
        self.task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = reply.await;
            let _ = this.update_in(cx, |flow, window, cx| match result {
                Some(Ok(())) => flow.dispatch(window, cx),
                Some(Err(e)) => flow.finish_in(FlowResult::Failed(error_text(&e)), window, cx),
                None => flow.finish_in(
                    FlowResult::Failed("the host key could not be recorded".to_owned()),
                    window,
                    cx,
                ),
            });
        }));
    }

    /// Confirms the init SQL shown, then opens again.
    pub fn confirm_init_sql(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Waiting::InitSql(token)) = self.waiting.take() else {
            return;
        };
        if let Some(run) = self.run.as_mut() {
            run.init_token = Some(token);
        }
        window.close_dialog(cx);
        self.dispatch(window, cx);
    }

    /// Cancels the prompt in progress ("Cancel" button): closes its dialog, clears what was
    /// typed and ends the run.
    pub fn cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.waiting.is_some() {
            window.close_dialog(cx);
        }
        self.finish_in(FlowResult::Cancelled, window, cx);
    }

    /// The prompt dialog was dismissed (Escape, the close button, a click outside): the run
    /// ends as cancelled, so the flow never stays stuck waiting on a dialog that is gone.
    /// Does nothing once the prompt was answered (its dialog is then closed by the flow).
    pub fn abandon(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.waiting.is_some() {
            self.finish_in(FlowResult::Cancelled, window, cx);
        }
    }

    /// Drops the prompt in progress, emptying its secret inputs first.
    fn clear_waiting(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.waiting.take() {
            Some(Waiting::Secrets { inputs, .. }) => {
                for input in &inputs {
                    secret_input::clear(input, window, cx);
                }
            }
            // Clears its master-password inputs; its Cancelled event is ignored (no longer
            // waiting).
            Some(Waiting::Vault(prompt)) => prompt.update(cx, |p, cx| p.dismissed(window, cx)),
            _ => {}
        }
    }

    fn finish_in(&mut self, result: FlowResult, window: &mut Window, cx: &mut Context<Self>) {
        let purpose = self.run.take().map_or(Purpose::Open, |r| r.purpose);
        self.clear_waiting(window, cx);
        self.prompt = PendingPrompt::None;
        let finished = FlowFinished { purpose, result };
        self.last = Some(finished.clone());
        cx.emit(finished);
        cx.notify();
    }
}

/// Shown with the session warnings when a file was opened read-only because it lies inside a
/// DBeaver workspace (`SessionInfo::inside_dbeaver_workspace`, I4).
pub const DBEAVER_READ_ONLY: &str =
    "Opened read-only: the file is inside a DBeaver workspace, which NineSQL never modifies.";

/// The user-facing text of a connect error. A locked vault gets its own sentence.
#[must_use]
pub fn error_text(e: &CoreError) -> String {
    match e {
        CoreError::Secret {
            source: SecretError::Locked(StoreKind::Vault),
            ..
        } => {
            // A lock that won a race with the open (the open itself answers NeedsVaultUnlock
            // when it finds the vault locked first).
            "The vault was locked while connecting: connect again and type the master password."
                .to_owned()
        }
        CoreError::Vault(_) | CoreError::NoVault(_) => super::vault::vault_error_text(e),
        other => other.to_string(),
    }
}

fn open_prompt_dialog(flow: Entity<ConnectFlow>, window: &mut Window, cx: &mut App) {
    let first = std::cell::Cell::new(true);
    window.open_dialog(cx, move |dialog, window, cx| {
        let dialog = {
            let flow = flow.clone();
            dialog.on_close(move |_, window, cx| {
                flow.update(cx, |f, cx| f.abandon(window, cx));
            })
        };
        let state = flow.read(cx);
        let prompt = state.prompt.clone();
        let inputs = state.secret_inputs();
        if first.replace(false)
            && let Some(input) = inputs.first()
        {
            input.focus_handle(cx).focus(window, cx);
        }
        let cancel = {
            let flow = flow.clone();
            Button::new("prompt-cancel")
                .label("Cancel")
                .on_click(move |_, window, cx| {
                    flow.update(cx, |f, cx| f.cancel(window, cx));
                })
        };
        let muted = cx.theme().muted_foreground;
        match prompt {
            PendingPrompt::Secrets(view) => {
                let ok = {
                    let flow = flow.clone();
                    Button::new("prompt-ok")
                        .primary()
                        .label("Connect")
                        .on_click(move |_, window, cx| {
                            flow.update(cx, |f, cx| f.submit_secrets(window, cx));
                        })
                };
                let body = v_flex().gap_2().children(
                    view.labels
                        .iter()
                        .zip(inputs.iter())
                        .enumerate()
                        .map(|(i, (label, input))| {
                            v_flex()
                                .id(("secret-prompt", i))
                                .gap_1()
                                .child(div().text_sm().child(label.clone()))
                                .child(secret_field(input))
                        }),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child("Kept in memory for this connection only; never written to disk."),
                );
                dialog
                    .title("Credentials")
                    .w(px(480.))
                    .child(body)
                    .footer(h_flex().gap_2().justify_end().child(cancel).child(ok))
            }
            PendingPrompt::HostKey(view) => {
                let ok = {
                    let flow = flow.clone();
                    Button::new("prompt-ok")
                        .warning()
                        .label("Trust this key and connect")
                        .on_click(move |_, window, cx| {
                            flow.update(cx, |f, cx| f.trust_host_key(window, cx));
                        })
                };
                let body = v_flex()
                    .gap_2()
                    .text_sm()
                    .child(format!(
                        "{} ({}) presented a host key NineSQL does not know.",
                        view.hop, view.address
                    ))
                    .child(div().id("hostkey-type").child(format!("Key type: {}", view.key_type)))
                    .child(
                        div()
                            .id("hostkey-fingerprint")
                            .font_family(cx.theme().mono_font_family.clone())
                            .child(view.fingerprint.clone()),
                    )
                    .when_some(view.other_key_types.clone(), |el, text| {
                        el.child(
                            div()
                                .id("hostkey-other-types")
                                .text_color(cx.theme().warning)
                                .child(text),
                        )
                    })
                    .child(div().text_xs().text_color(muted).child(
                        "Compare the fingerprint with the one the server administrator gives you. No credential was sent to this host.",
                    ));
                dialog
                    .title("Unknown SSH host key")
                    .w(px(560.))
                    .child(body)
                    .footer(h_flex().gap_2().justify_end().child(cancel).child(ok))
            }
            PendingPrompt::InitSql(view) => {
                let ok = {
                    let flow = flow.clone();
                    Button::new("prompt-ok")
                        .danger()
                        .label("Run and connect")
                        .on_click(move |_, window, cx| {
                            flow.update(cx, |f, cx| f.confirm_init_sql(window, cx));
                        })
                };
                let warn = cx.theme().warning;
                let mono = cx.theme().mono_font_family.clone();
                let body = v_flex()
                    .gap_2()
                    .text_sm()
                    .when(view.production, |el| {
                        let color = hsla(ninesql_core::ConnectionType::production().color);
                        el.border_2().border_color(color).p_2()
                    })
                    .when(view.imported, |el| {
                        el.child(
                            div()
                                .id("init-sql-imported")
                                .text_color(warn)
                                .child(IMPORTED_INIT_SQL_NOTICE),
                        )
                    })
                    .child(format!(
                        "\"{}\" runs these statements at every connect. {} of them need confirmation:",
                        view.connection_name,
                        view.flagged()
                    ))
                    .children(view.rows.iter().map(|row| {
                        v_flex()
                            .id(("init-sql", row.number))
                            .child(
                                div()
                                    .font_family(mono.clone())
                                    .child(format!("{}. {}", row.number, row.text)),
                            )
                            .when_some(row.flag.clone(), |el, flag| {
                                el.child(div().text_xs().text_color(warn).child(format!("Needs confirmation: {flag}")))
                            })
                    }));
                dialog
                    .title("Confirm init SQL")
                    .w(px(640.))
                    .child(body)
                    .footer(h_flex().gap_2().justify_end().child(cancel).child(ok))
            }
            PendingPrompt::None | PendingPrompt::Running | PendingPrompt::VaultUnlock => {
                dialog.title("Connecting…")
            }
        }
    });
}

/// What runs when the user confirms a statement.
pub type ConfirmCallback = Rc<dyn Fn(&mut Window, &mut App)>;

/// Opens the production statement confirmation (ADR-0007 §3): the exact statement, the
/// connection name in the type color and the reason. `on_confirm` resends the statement with
/// the token.
///
/// INTEGRATION POINT (P3): called on `ExecEvent::NeedsConfirmation` once the editor executes
/// statements.
pub fn open_statement_confirmation(
    view: StatementConfirmView,
    type_color: Hsla,
    on_confirm: ConfirmCallback,
    window: &mut Window,
    cx: &mut App,
) {
    window.open_dialog(cx, move |dialog, _window, cx| {
        let mono = cx.theme().mono_font_family.clone();
        let muted = cx.theme().muted_foreground;
        let ok = {
            let on_confirm = on_confirm.clone();
            Button::new("statement-run")
                .danger()
                .label("Run")
                .on_click(move |_, window, cx| {
                    window.close_dialog(cx);
                    on_confirm(window, cx);
                })
        };
        let cancel = Button::new("statement-cancel")
            .label("Cancel")
            .on_click(|_, window, cx| window.close_dialog(cx));
        let body = v_flex()
            .gap_2()
            .text_sm()
            .border_2()
            .border_color(type_color)
            .p_2()
            .child(
                div()
                    .id("statement-connection")
                    .px_1()
                    .bg(type_color)
                    .text_color(text_on(type_color))
                    .child(view.connection_name.clone()),
            )
            .child(div().child(format!(
                "This statement needs confirmation ({}).",
                view.reason
            )))
            .child(
                div()
                    .id("statement-text")
                    .font_family(mono)
                    .child(view.statement.clone()),
            )
            .when(view.params > 0, |el| {
                el.child(div().text_xs().text_color(muted).child(format!(
                    "{} bind parameters (values not shown).",
                    view.params
                )))
            });
        dialog
            .title(if view.production {
                "Run on production?"
            } else {
                "Run this statement?"
            })
            .w(px(640.))
            .child(body)
            .footer(h_flex().gap_2().justify_end().child(cancel).child(ok))
    });
}
