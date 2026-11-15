//! "Enable connection" (P2-B): shows why a connection is disabled and enables it through
//! `Workspace::enable_connection` (via [`ConnectionHub::enable`]). The core clears a disabled
//! reason only when its cause is gone (a driver that accepts the connection); a `Route` reason
//! also needs `route_fixed = true`, which the prompt passes only after the user ticked the
//! explicit "I set up the route again" confirmation.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use ninesql_core::{ConnectionId, DisabledKind, DisabledReason};

use super::model::{ROUTE_CONFIRMATION, disabled_kind_label};
use crate::appearance::{MIN_TEXT_CONTRAST, ensure_contrast};
use crate::connections::hub::ConnectionHub;

/// The text of a connection's disabled state, for the navigator and the prompt; `None` when it
/// is enabled.
#[must_use]
pub fn disabled_text(enabled: bool, reason: Option<&DisabledReason>) -> Option<String> {
    if enabled {
        return None;
    }
    Some(match reason {
        Some(r) => format!("Disabled ({}): {}", disabled_kind_label(r.kind), r.message),
        None => "Disabled by you.".to_owned(),
    })
}

/// Emitted when the connection was enabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectionEnabled(pub ConnectionId);

/// The prompt.
pub struct EnablePrompt {
    hub: Entity<ConnectionHub>,
    id: ConnectionId,
    name: String,
    reason: Option<DisabledReason>,
    route_fixed: bool,
    error: Option<String>,
    busy: bool,
    _task: Option<Task<()>>,
}

impl EventEmitter<ConnectionEnabled> for EnablePrompt {}

impl EnablePrompt {
    /// A prompt for connection `id` (read from the hub).
    pub fn new(hub: Entity<ConnectionHub>, id: ConnectionId, cx: &mut Context<Self>) -> Self {
        let (name, reason) = hub
            .read(cx)
            .connection(id)
            .map(|c| (c.name.clone(), c.disabled_reason.clone()))
            .unwrap_or_default();
        Self {
            hub,
            id,
            name,
            reason,
            route_fixed: false,
            error: None,
            busy: false,
            _task: None,
        }
    }

    /// The reason needs the "I set up the route again" confirmation.
    #[must_use]
    pub fn needs_route_confirmation(&self) -> bool {
        self.reason
            .as_ref()
            .is_some_and(|r| r.kind == DisabledKind::Route)
    }

    /// Whether the user confirmed the route.
    #[must_use]
    pub fn route_fixed(&self) -> bool {
        self.route_fixed
    }

    /// The last refusal.
    #[must_use]
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Whether "Enable" can be pressed.
    #[must_use]
    pub fn can_submit(&self) -> bool {
        !self.busy && (!self.needs_route_confirmation() || self.route_fixed)
    }

    /// Ticks or unticks the route confirmation.
    pub fn toggle_route_fixed(&mut self, cx: &mut Context<Self>) {
        self.route_fixed = !self.route_fixed;
        cx.notify();
    }

    /// "Enable": the core decides; a refusal is shown in the prompt.
    pub fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_submit() {
            if self.needs_route_confirmation() && !self.route_fixed {
                self.error = Some("Confirm that the route was set up again first.".to_owned());
                cx.notify();
            }
            return;
        }
        // Only a Route reason takes the confirmation; any other reason never passes it.
        let route_fixed = self.needs_route_confirmation() && self.route_fixed;
        let id = self.id;
        let task = self
            .hub
            .update(cx, |hub, cx| hub.enable(id, route_fixed, cx));
        self.busy = true;
        self.error = None;
        cx.notify();
        self._task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |p, window, cx| p.on_result(result, window, cx));
        }));
    }

    /// Applies the core's answer (public for tests).
    pub fn on_result(
        &mut self,
        result: Result<(), String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.busy = false;
        match result {
            Ok(()) => {
                window.close_dialog(cx);
                cx.emit(ConnectionEnabled(self.id));
            }
            Err(e) => self.error = Some(format!("Not enabled: {e}")),
        }
        cx.notify();
    }
}

impl Render for EnablePrompt {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let bg = cx.theme().background;
        let muted = cx.theme().muted_foreground;
        let danger = ensure_contrast(cx.theme().danger, &[bg], MIN_TEXT_CONTRAST);
        let route = self.needs_route_confirmation();
        let why = disabled_text(false, self.reason.as_ref()).unwrap_or_default();
        v_flex()
            .key_context("EnableConnection")
            .gap_2()
            .text_sm()
            .child(format!("\"{}\" is disabled.", self.name))
            .child(div().id("enable-reason").child(why))
            .when(route, |el| {
                el.child(div().text_xs().text_color(muted).child(
                    "The import could not reproduce how DBeaver reached this server (a network profile, a proxy or another SSH forwarding target). Connecting without it would reach the server another way. Edit the connection (F4) to set up the SSH tunnel first.",
                ))
                .child(
                    Checkbox::new("enable-route-fixed")
                        .checked(self.route_fixed)
                        .label(ROUTE_CONFIRMATION)
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_route_fixed(cx))),
                )
            })
            .when_some(self.error.clone(), |el, e| {
                el.child(div().id("enable-error").text_color(danger).child(e))
            })
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("enable-cancel")
                            .label("Cancel")
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("enable-ok")
                            .primary()
                            .label("Enable")
                            .disabled(!self.can_submit())
                            .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
                    ),
            )
    }
}

/// Opens the prompt in a dialog and returns it (subscribe to [`ConnectionEnabled`]).
pub fn open_enable_prompt(
    hub: Entity<ConnectionHub>,
    id: ConnectionId,
    window: &mut Window,
    cx: &mut App,
) -> Entity<EnablePrompt> {
    let prompt = cx.new(|cx| EnablePrompt::new(hub, id, cx));
    let view = prompt.clone();
    window.open_dialog(cx, move |dialog, _window, _cx| {
        dialog
            .title("Enable connection")
            .w(px(560.))
            .child(view.clone())
    });
    prompt
}
