//! The `ninesql` binary: registers the drivers NineSQL ships in the core's `DriverRegistry`
//! (L5) and starts the application. Only this file names driver crates; the library reaches
//! drivers through `ninesql-core` (docs/02-architecture.md).
//!
//! `--version` and `--help` are answered before GPUI starts, so they need no display, no GPU
//! and no workspace: the package install tests (`packaging/`, P6-A) run them in containers.
#![forbid(unsafe_code)]

use std::io::Write;
use std::process::ExitCode;
use std::sync::Arc;

use ninesql_core::DriverRegistry;

const USAGE: &str = "\
Usage: ninesql [OPTION]

Native database client for Debian and Ubuntu. Run without options to open a window.

Options:
  -h, --help       Print this help and exit
  -V, --version    Print the version and exit
";

/// What the command line asks for.
#[derive(Debug, PartialEq, Eq)]
enum Command {
    Run,
    Version,
    Help,
    /// An argument NineSQL does not take; it is not echoed back (it could be anything).
    Invalid,
}

fn parse<I: IntoIterator<Item = std::ffi::OsString>>(args: I) -> Command {
    let mut args = args.into_iter();
    let Some(first) = args.next() else {
        return Command::Run;
    };
    if args.next().is_some() {
        return Command::Invalid;
    }
    match first.to_str() {
        Some("-V" | "--version") => Command::Version,
        Some("-h" | "--help") => Command::Help,
        _ => Command::Invalid,
    }
}

fn main() -> ExitCode {
    match parse(std::env::args_os().skip(1)) {
        Command::Version => {
            // A closed stdout is not worth an error for `--version`.
            let _ = writeln!(
                std::io::stdout().lock(),
                "ninesql {}",
                env!("CARGO_PKG_VERSION")
            );
            return ExitCode::SUCCESS;
        }
        Command::Help => {
            let _ = write!(std::io::stdout().lock(), "{USAGE}");
            return ExitCode::SUCCESS;
        }
        Command::Invalid => {
            let _ = write!(
                std::io::stderr().lock(),
                "ninesql: unexpected argument\n\n{USAGE}"
            );
            return ExitCode::from(2);
        }
        Command::Run => {}
    }

    let mut registry = DriverRegistry::new();
    registry.register(Arc::new(ninesql_driver_pgwire::PgWireDriver::new()));
    registry.register(Arc::new(ninesql_driver_mysqlwire::MySqlWireDriver::new()));
    registry.register(Arc::new(ninesql_driver_sqlite::SqliteDriver::new()));
    ninesql_ui::app::run(registry);
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::{Command, parse};

    fn args(list: &[&str]) -> Vec<std::ffi::OsString> {
        list.iter().map(std::ffi::OsString::from).collect()
    }

    #[test]
    fn no_argument_runs_the_application() {
        assert_eq!(parse(args(&[])), Command::Run);
    }

    #[test]
    fn version_and_help_flags() {
        assert_eq!(parse(args(&["--version"])), Command::Version);
        assert_eq!(parse(args(&["-V"])), Command::Version);
        assert_eq!(parse(args(&["--help"])), Command::Help);
        assert_eq!(parse(args(&["-h"])), Command::Help);
    }

    #[test]
    fn anything_else_is_refused() {
        assert_eq!(parse(args(&["--verbose"])), Command::Invalid);
        assert_eq!(parse(args(&["file.sql"])), Command::Invalid);
        assert_eq!(parse(args(&["--version", "--help"])), Command::Invalid);
    }
}
