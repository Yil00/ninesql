//! NineSQL desktop application (GPUI): the shell of ROADMAP P1-A.
//!
//! - [`workspace`]: main window content: navigator dock, editor tabs and splits, status bar
//!   (F-WS-01); one workspace per window, any number of windows.
//! - [`actions`], [`keymap`], [`palette`]: every user-facing action with a default key binding
//!   and a command palette entry; bindings overridable in settings.toml (F-WS-02).
//! - [`appearance`], [`settings`]: light / dark themes, fonts, fractional scaling (F-WS-03);
//!   [`persist`] saves changes made from the UI back to settings.toml.
//! - [`services`]: the seam to `ninesql-core` services (driver runtime: P1-D).
//! - [`import`]: the DBeaver import flow (P2-B): first-run offer, preview, apply, outcome.
//! - [`navigator`], [`browse`]: the metadata tree, navigator filters, "Find object", the object
//!   editor, "Generate SQL" and the editor's object completion (P4-B).
//!
//! GPUI comes from `gpui-kit` under the controls of ADR-0013. GPUI's credentials and HTTP
//! client APIs are never used: secrets go through `ninesql-secrets` only (I1), and NineSQL has
//! no network traffic of its own (I2).
#![forbid(unsafe_code)]

pub mod actions;
pub mod app;
pub mod appearance;
pub mod browse;
pub mod connections;
pub mod editor_tab;
pub mod import;
pub mod keymap;
pub mod layout;
pub mod navigator;
pub mod palette;
pub mod persist;
pub mod services;
pub mod settings;
pub mod snippets;
pub mod transfer;
pub mod workspace;

#[cfg(test)]
mod tests;
