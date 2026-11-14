//! Seam between the shell and `ninesql-core` services (P1-D).
//!
//! The driver runtime, sessions and secrets are built in P1-D on the Tokio runtime
//! (ADR-0004); the UI only ever talks to them through `ninesql-core` handles whose results come
//! back over channels, so the foreground thread never awaits I/O (I6). Until that lands, the
//! shell holds a [`CoreBridge`] that reports "no connection". P1-C / P1-D replace
//! [`Disconnected`] with the real bridge in [`crate::app::init`]; views read only this trait.

use std::rc::Rc;

use gpui_kit::{App, Global, SharedString};

/// What the shell needs from `ninesql-core` today. It grows with P1-C (connections) and P1-D
/// (sessions); every method must return immediately (no I/O on the foreground thread).
pub trait CoreBridge: 'static {
    /// Short label of the connection of the active editor for the status bar, `None` when there
    /// is none. Never contains a secret.
    fn active_connection_label(&self) -> Option<SharedString>;
}

/// The bridge used until P1-D: no driver runtime, no connection.
#[derive(Debug, Default)]
pub struct Disconnected;

impl CoreBridge for Disconnected {
    fn active_connection_label(&self) -> Option<SharedString> {
        None
    }
}

/// The bridge in use, as a GPUI global.
#[derive(Clone)]
pub struct Services(pub Rc<dyn CoreBridge>);

impl Global for Services {}

impl Services {
    /// The bridge of the application; [`Disconnected`] if none was installed.
    #[must_use]
    pub fn bridge(cx: &App) -> Rc<dyn CoreBridge> {
        cx.try_global::<Self>()
            .map(|s| s.0.clone())
            .unwrap_or_else(|| Rc::new(Disconnected))
    }
}
