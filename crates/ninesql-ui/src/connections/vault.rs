//! The vault's master-password prompt (ADR-0005): unlock it when an open answers
//! `OpenOutcome::NeedsVaultUnlock` or before saving a password in it, or create it the first
//! time a password is saved to the vault.
//!
//! The master password is typed into [`secret_input`] fields (masked, no reveal, no copy, not
//! exposed to accessibility clients), read into a zeroized buffer, moved into
//! `CoreServices::unlock_vault` / `create_vault` (which run Argon2id on the driver runtime's
//! blocking pool, I6) and the inputs are cleared at once. A lock that arrives during the unlock
//! wins (the core answers `VaultError::Locked`): the prompt says so and stays open.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::InputState;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use ninesql_core::CoreError;
use ninesql_core::secrets::{ExposeSecret as _, VaultError};

use super::hub::ConnectionHub;
use super::secret_input::{self, masked_input, read_secret, secret_field};

/// What the prompt does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VaultMode {
    /// Unlock the existing vault.
    Unlock,
    /// Create the vault (first password saved to it).
    Create,
}

/// How the prompt ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VaultPromptEvent {
    /// The vault is unlocked (or created and unlocked).
    Done,
    /// The user cancelled or dismissed the prompt.
    Cancelled,
}

/// The master-password prompt.
pub struct VaultPrompt {
    hub: Entity<ConnectionHub>,
    mode: VaultMode,
    master: Entity<InputState>,
    confirm: Entity<InputState>,
    error: Option<String>,
    busy: bool,
    finished: bool,
    _task: Option<Task<()>>,
}

impl EventEmitter<VaultPromptEvent> for VaultPrompt {}

impl VaultPrompt {
    /// A prompt for `mode`.
    pub fn new(
        hub: Entity<ConnectionHub>,
        mode: VaultMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            hub,
            mode,
            master: masked_input(window, cx, "master password"),
            confirm: masked_input(window, cx, "master password again"),
            error: None,
            busy: false,
            finished: false,
            _task: None,
        }
    }

    /// Unlock or create.
    #[must_use]
    pub fn mode(&self) -> VaultMode {
        self.mode
    }

    /// The last error shown.
    #[must_use]
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Whether the core is deriving the key.
    #[must_use]
    pub fn is_busy(&self) -> bool {
        self.busy
    }

    /// The master-password input, then the confirmation (create only), for tests.
    #[must_use]
    pub fn inputs(&self) -> (Entity<InputState>, Entity<InputState>) {
        (self.master.clone(), self.confirm.clone())
    }

    fn clear(&self, window: &mut Window, cx: &mut App) {
        secret_input::clear(&self.master, window, cx);
        secret_input::clear(&self.confirm, window, cx);
    }

    /// Unlocks or creates the vault with what was typed; the inputs are cleared at once.
    pub fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy || self.finished {
            return;
        }
        let master = read_secret(&self.master, cx);
        let confirm = read_secret(&self.confirm, cx);
        self.clear(window, cx);
        let Some(master) = master else {
            self.error = Some("Type the master password.".to_owned());
            cx.notify();
            return;
        };
        if self.mode == VaultMode::Create
            && confirm.as_ref().map(|c| c.expose_secret()) != Some(master.expose_secret())
        {
            self.error = Some("The two passwords differ: type them again.".to_owned());
            cx.notify();
            return;
        }
        drop(confirm);
        let Some(core) = self.hub.read(cx).core().cloned() else {
            self.error = Some("The vault is not available in this session.".to_owned());
            cx.notify();
            return;
        };
        let reply = match self.mode {
            VaultMode::Unlock => core.unlock_vault(master),
            VaultMode::Create => core.create_vault(master),
        };
        self.busy = true;
        self.error = None;
        cx.notify();
        self._task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = reply.await;
            let _ = this.update_in(cx, |p, window, cx| p.on_result(result, window, cx));
        }));
    }

    fn on_result(
        &mut self,
        result: Option<Result<(), CoreError>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.busy = false;
        match result {
            Some(Ok(())) => {
                self.finished = true;
                self.hub.update(cx, |h, cx| h.mark_unlocked(cx));
                window.close_dialog(cx);
                cx.emit(VaultPromptEvent::Done);
            }
            Some(Err(e)) => self.error = Some(vault_error_text(&e)),
            None => self.error = Some("The vault did not answer; try again.".to_owned()),
        }
        cx.notify();
    }

    /// "Cancel": clears the inputs, closes the dialog and reports [`VaultPromptEvent::Cancelled`].
    pub fn cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.finished {
            return;
        }
        window.close_dialog(cx);
        self.dismissed(window, cx);
    }

    /// The dialog was dismissed (Escape, close button, click outside).
    pub fn dismissed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.clear(window, cx);
        cx.emit(VaultPromptEvent::Cancelled);
    }
}

/// User-facing text of a vault error; never contains the master password.
#[must_use]
pub fn vault_error_text(e: &CoreError) -> String {
    match e {
        CoreError::Vault(VaultError::WrongPassword) => "Wrong master password.".to_owned(),
        CoreError::Vault(VaultError::Locked) => "The secrets were locked again while the vault was being unlocked (sleep, screen lock or \"Lock secrets now\"): type the master password again.".to_owned(),
        CoreError::Vault(VaultError::TooManyAttempts { retry_after }) => format!(
            "Too many wrong attempts: try again in {} s.",
            retry_after.as_secs().max(1)
        ),
        CoreError::Vault(VaultError::NotCreated) => {
            "There is no vault yet: save a password in the vault to create it.".to_owned()
        }
        CoreError::Vault(VaultError::AlreadyExists(_)) => {
            "A vault already exists: unlock it instead.".to_owned()
        }
        CoreError::Vault(VaultError::EmptyPassword) => {
            "The master password must not be empty.".to_owned()
        }
        CoreError::NoVault(detail) => format!("The vault cannot be used: {detail}."),
        other => other.to_string(),
    }
}

impl Render for VaultPrompt {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let danger = crate::appearance::ensure_contrast(
            cx.theme().danger,
            &[cx.theme().background],
            crate::appearance::MIN_TEXT_CONTRAST,
        );
        let create = self.mode == VaultMode::Create;
        v_flex()
            .key_context("VaultPrompt")
            .gap_2()
            .text_sm()
            .child(if create {
                "Passwords are saved in an encrypted vault on this computer. Choose its master password: it is never saved, and without it the saved passwords cannot be recovered."
            } else {
                "The vault is locked. Type its master password to use the passwords saved in it."
            })
            .child(secret_field(&self.master))
            .when(create, |el| el.child(secret_field(&self.confirm)))
            .when_some(self.error.clone(), |el, e| {
                el.child(div().id("vault-error").text_color(danger).child(e))
            })
            .when(self.busy, |el| {
                el.child(div().text_xs().text_color(muted).child("Checking…"))
            })
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("vault-cancel")
                            .label("Cancel")
                            .on_click(cx.listener(|this, _, window, cx| this.cancel(window, cx))),
                    )
                    .child(
                        Button::new("vault-ok")
                            .primary()
                            .label(if create { "Create vault" } else { "Unlock" })
                            .disabled(self.busy)
                            .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
                    ),
            )
    }
}

/// Opens the prompt in a dialog and returns it (subscribe to [`VaultPromptEvent`]).
pub fn open_vault_prompt(
    hub: Entity<ConnectionHub>,
    mode: VaultMode,
    window: &mut Window,
    cx: &mut App,
) -> Entity<VaultPrompt> {
    let prompt = cx.new(|cx| VaultPrompt::new(hub, mode, window, cx));
    let view = prompt.clone();
    let first = std::cell::Cell::new(true);
    window.open_dialog(cx, move |dialog, window, cx| {
        if first.replace(false) {
            let input = view.read(cx).master.clone();
            input.focus_handle(cx).focus(window, cx);
        }
        let on_close = view.clone();
        dialog
            .title(match mode {
                VaultMode::Unlock => "Unlock the vault",
                VaultMode::Create => "Create the vault",
            })
            .w(px(480.))
            .on_close(move |_, window, cx| {
                on_close.update(cx, |p, cx| p.dismissed(window, cx));
            })
            .child(view.clone())
    });
    prompt
}
