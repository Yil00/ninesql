//! NineSQL desktop application (GPUI): the shell of ROADMAP P1-A.
//!
//! - [`workspace`]: main window content: navigator dock, editor tabs and splits, status bar
//!   (F-WS-01); one workspace per window, any number of windows.
//! - [`actions`], [`keymap`], [`palette`]: every user-facing action with a default key binding
//!   and a command palette entry; bindings overridable in settings.toml (F-WS-02).
//! - [`appearance`], [`settings`]: light / dark themes, fonts, fractional scaling (F-WS-03).
//! - [`services`]: the seam to `ninesql-core` services (driver runtime: P1-D).
//!
//! GPUI comes from `gpui-kit` under the controls of ADR-0013. GPUI's credentials and HTTP
//! client APIs are never used: secrets go through `ninesql-secrets` only (I1), and NineSQL has
//! no network traffic of its own (I2).
#![forbid(unsafe_code)]

pub mod actions;
pub mod app;
pub mod appearance;
pub mod editor_tab;
pub mod keymap;
pub mod layout;
pub mod navigator;
pub mod palette;
pub mod services;
pub mod settings;
pub mod workspace;

// Dependency graph of docs/02-architecture.md, made explicit until the UI uses these crates.
use ninesql_editor as _;
use ninesql_grid as _;

#[cfg(test)]
mod tests;
