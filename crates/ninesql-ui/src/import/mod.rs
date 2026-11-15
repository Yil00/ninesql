//! DBeaver import in the UI (ROADMAP P2-B, F-CON-14, ADR-0006): the entry points, the preview
//! screen, the apply with every `ImportError`, and the outcome with the report file.
//!
//! - [`model`]: the preview and outcome view models, built from the core's `ImportPreview`
//!   and `ImportOutcome` without I/O, and the user's choices as `ImportChoices`.
//! - [`flow`]: the per-window flow and its screen (preview on the driver runtime, I6; the
//!   vault prompts; "Test" through the connect flow).
//! - [`enable`]: "Enable connection" for connections imported disabled, with the explicit
//!   "I set up the route again" confirmation for a route reason.
//!
//! The UI never opens a DBeaver file (I4): the core reads them, read-only. No secret value is
//! shown, logged or kept here (I1): secrets are counts and credential fields.

pub mod enable;
pub mod flow;
pub mod model;

#[cfg(test)]
mod tests;
