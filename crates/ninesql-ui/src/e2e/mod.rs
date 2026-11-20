//! The end-to-end scenario runner of the test build (feature `e2e-scenarios`, off by default,
//! Linux only; [ADR-0029](../../../../docs/adr/0029-automated-ui-end-to-end-tests-and-application-logging.md)
//! §2). Never in a package: `make e2e-feature-guard`, the compile-time refusal and the marker of
//! `src/main.rs`, and the package checks of `packaging/` keep it out.
//!
//! `ninesql --e2e-run <scenario.toml> --e2e-out <directory>` (only in this build):
//!
//! 1. [`isolation::check`]: `HOME` and the four XDG directories inside one fresh `0700` run
//!    directory under the temporary directory or `target/`, not the account's home; otherwise
//!    status 2 before anything is read or written.
//! 2. [`scenario::Scenario::load`]: the scenario file, checked whole (actions registered, probe
//!    keys known, paths inside the repository); otherwise status 2.
//! 3. `ninesql_core::e2e::install(run_dir)`: the core's guards (loopback endpoints only, files
//!    in the run directory, the vault forced, no `secret-service` build); otherwise status 2.
//! 4. The dev engines the scenario names must answer, otherwise the run is skipped (status 5).
//!    The run's HOME gets its SQLite files, DBeaver fixture copies and small files.
//! 5. The application starts in a real window titled `… [E2E TEST BUILD]`, the core creates the
//!    run's vault with a random master password (never written anywhere), the runner saves the
//!    connections and runs the steps ([`runner`]).
//! 6. `results.json` is written in the output directory, then every file of the run directory
//!    is scanned for the passwords of `dev/engines.json` and the master password (I1).
//!
//! The runner opens no port, socket, D-Bus name or pipe, reads nothing at run time but its
//! scenario file, `dev/engines.json`, the files the scenario names in the repository and its
//! own run directory, and takes no input during a run; its only output channel besides files is
//! standard output (the checkpoint lines).

pub mod isolation;
pub mod probe;
pub mod runner;
pub mod scenario;
pub mod setup;

mod app;

#[cfg(test)]
mod headless;
#[cfg(test)]
mod tests;

pub use app::{Prepared, Stopped, prepare, results_json};

/// Exit status of a scenario run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ExitStatus {
    /// Every step passed and the I1 scan found nothing.
    Pass = 0,
    /// A step failed or timed out (or the whole-run timeout was reached).
    Fail = 1,
    /// Refused before anything was read or written: arguments, isolation, the scenario file,
    /// the core's guards (a `secret-service` build included).
    Refused = 2,
    /// The I1 scan found a password of `dev/engines.json` or the master password in a file.
    SecretFound = 3,
    /// The setup failed after the start: files of the run's HOME, the core, the vault, a
    /// connection that could not be saved, the window.
    SetupError = 4,
    /// A dev engine the scenario needs did not answer; nothing ran.
    Skipped = 5,
}

impl ExitStatus {
    /// The process exit code.
    #[must_use]
    pub const fn code(self) -> u8 {
        self as u8
    }

    /// The `outcome` of `results.json`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Refused => "refused",
            Self::SecretFound => "secret-found",
            Self::SetupError => "setup-error",
            Self::Skipped => "skipped",
        }
    }
}

/// `schema_version` of `results.json`.
pub const RESULTS_SCHEMA_VERSION: u32 = 1;
