//! Connections (ROADMAP P1-C): the connection dialog and its view model, the connection list of
//! the navigator, the connect flow with every `OpenOutcome`, and the production visuals of
//! ADR-0007.
//!
//! - [`engines`]: what the dialog offers per engine, from the registry and the declared
//!   capabilities (I8).
//! - [`url`]: connection URLs parsed into fields (F-CON-12); a URL with a password is refused.
//! - [`form`]: the dialog's view model and its notices (I5, ADR-0016, ADR-0019).
//! - [`flow`]: view models of the prompts and warnings of the connect flow.
//! - [`hub`]: the application's connections, sessions and core services (I6).
//! - [`connect_flow`]: the per-window connect flow and its prompt dialogs.
//! - [`dialog`]: the connection dialog view.
//! - [`secret_input`]: masked secret inputs, read into zeroized buffers and cleared on exit.
//! - [`vault`]: the vault's master-password prompt (unlock, create).
//! - [`visuals`]: type color, type / read-only / security badges (ADR-0007 §4).

pub mod connect_flow;
pub mod dialog;
pub mod engines;
pub mod flow;
pub mod form;
pub mod hub;
pub mod secret_input;
pub mod url;
pub mod vault;
pub mod visuals;

#[cfg(test)]
mod tests;
