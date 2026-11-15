//! Masked inputs for secrets (passwords, tokens, passphrases), shared by the connection dialog
//! and the connect flow's credential prompt.
//!
//! - Always masked, with no "show" toggle: the value is never drawn, not even on request.
//! - Rendered with [`InputContentType::Password`], so the widget neither copies nor cuts the
//!   value to the clipboard (a masked state is not copyable) and does not publish it to
//!   accessibility clients.
//! - Read only through [`read_secret`], which copies the input's text into a zeroized buffer
//!   and hands it over as a [`SecretString`], never as a `SharedString` or a plain `String`.
//! - Cleared with [`clear`] on every exit path (save, cancel, close, failed or abandoned
//!   prompt). `set_value` also drops the undo history, which would otherwise keep the text.
//!
//! The widget's own buffer (a rope) is not zeroized when it is replaced or dropped: that
//! limitation of gpui-component is in the P1-C task report.

use gpui_kit::component::input::{Input, InputContentType, InputState};
use gpui_kit::{App, AppContext as _, Entity, Window};
use ninesql_core::secrets::SecretString;
use zeroize::Zeroizing;

/// A new masked input for a secret.
pub fn masked_input(window: &mut Window, cx: &mut App, placeholder: &str) -> Entity<InputState> {
    let placeholder = placeholder.to_owned();
    cx.new(|cx| {
        InputState::new(window, cx)
            .masked(true)
            .placeholder(placeholder)
    })
}

/// The element of a secret input: masked, password content type, no mask toggle.
#[must_use]
pub fn secret_field(state: &Entity<InputState>) -> Input {
    Input::new(state).content_type(InputContentType::Password)
}

/// The typed secret, or `None` when the input is empty. The text goes straight from the input's
/// rope into a buffer sized exactly to it (so turning it into a [`SecretString`] does not leave a
/// reallocated copy behind) and is zeroized on drop.
#[must_use]
pub fn read_secret(state: &Entity<InputState>, cx: &App) -> Option<SecretString> {
    let rope = state.read(cx).text();
    let len = rope.len();
    if len == 0 {
        return None;
    }
    let mut buf = Zeroizing::new(String::with_capacity(len));
    for chunk in rope.chunks() {
        buf.push_str(chunk);
    }
    Some(SecretString::from(std::mem::take(&mut *buf)))
}

/// Empties a secret input and its undo history.
pub fn clear(state: &Entity<InputState>, window: &mut Window, cx: &mut App) {
    state.update(cx, |s, cx| {
        if s.text().len() > 0 {
            s.set_value("", window, cx);
        }
    });
}

/// Whether a secret input is empty (tests and the "type a value" hints).
#[must_use]
pub fn is_empty(state: &Entity<InputState>, cx: &App) -> bool {
    state.read(cx).text().len() == 0
}
