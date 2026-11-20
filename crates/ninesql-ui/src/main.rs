//! The `ninesql` binary: registers the drivers NineSQL ships in the core's `DriverRegistry`
//! (L5) and starts the application. Only this file names driver crates; the library reaches
//! drivers through `ninesql-core` (docs/02-architecture.md).
//!
//! `--version` and `--help` are answered before GPUI starts, so they need no display, no GPU
//! and no workspace: the package install tests (`packaging/`, P6-A) run them in containers.
//!
//! The first thing `main` does is install a process-wide panic hook ([`install_panic_hook`],
//! P6-E review M2): a panic message can carry anything the panicking code formatted (a value,
//! a statement, a secret), and the default hook prints it to stderr, where a terminal log, a
//! journal or a bug report keeps it (I1). Release builds print only a fixed message and the
//! panic's source location; the message is kept only in debug builds.
#![forbid(unsafe_code)]

use std::io::Write;
use std::process::ExitCode;
use std::sync::Arc;

use ninesql_core::DriverRegistry;

mod logging;

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

/// What a panic report may include. The payload variant exists only in debug builds, so a
/// release binary has no code path that formats a panic message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PanicReport {
    /// Fixed text and the source location only. Debug builds construct it only in tests.
    #[cfg_attr(debug_assertions, allow(dead_code))]
    Release,
    /// Also the panic message (and a backtrace when `RUST_BACKTRACE` asks for one).
    #[cfg(debug_assertions)]
    Debug,
}

impl PanicReport {
    const fn for_this_build() -> Self {
        #[cfg(debug_assertions)]
        {
            PanicReport::Debug
        }
        #[cfg(not(debug_assertions))]
        {
            PanicReport::Release
        }
    }
}

/// The text the panic hook writes to stderr. `location` is `file:line:column` of the panic
/// (a source path fixed at compile time, remapped in release builds); `payload` is read only
/// in [`PanicReport::Debug`].
fn format_panic_report(
    mode: PanicReport,
    location: Option<&std::panic::Location<'_>>,
    payload: &(dyn std::any::Any + Send),
) -> String {
    let at = match location {
        Some(location) => format!(
            "{}:{}:{}",
            location.file(),
            location.line(),
            location.column()
        ),
        None => "an unknown location".to_owned(),
    };
    let mut report = format!("ninesql: internal error (panic) at {at}\n");
    match mode {
        PanicReport::Release => {
            let _ = payload;
            report.push_str(
                "The panic message is not shown because it could contain private data; \
                 please include this location when you report the problem.\n",
            );
        }
        #[cfg(debug_assertions)]
        PanicReport::Debug => {
            let message = payload
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                .unwrap_or("(non-string panic payload)");
            report.push_str("message (debug build only): ");
            report.push_str(message);
            report.push('\n');
        }
    }
    report
}

/// Replaces the default panic hook for the whole process, so panics on every thread (the GPUI
/// foreground, the driver runtime's workers, the sqlite session threads…) go through it. It is
/// installed before any thread is spawned; nothing else in the workspace sets a hook. It never
/// panics itself and ignores a closed stderr.
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let mode = PanicReport::for_this_build();
        #[allow(unused_mut)]
        let mut report = format_panic_report(mode, info.location(), info.payload());
        #[cfg(debug_assertions)]
        {
            let backtrace = std::backtrace::Backtrace::capture();
            if backtrace.status() == std::backtrace::BacktraceStatus::Captured {
                report.push_str(&format!("backtrace:\n{backtrace}\n"));
            }
        }
        let _ = std::io::stderr().lock().write_all(report.as_bytes());
    }));
}

fn main() -> ExitCode {
    install_panic_hook();
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

    // The subscriber (ADR-0029 §3): before GPUI starts, after the panic hook. Its guard flushes
    // the file log when `main` returns.
    let _log = logging::install(ninesql_core::storage::Paths::from_env().ok().as_ref());

    let mut registry = DriverRegistry::new();
    registry.register(Arc::new(ninesql_driver_pgwire::PgWireDriver::new()));
    registry.register(Arc::new(ninesql_driver_mysqlwire::MySqlWireDriver::new()));
    registry.register(Arc::new(ninesql_driver_sqlite::SqliteDriver::new()));
    ninesql_ui::app::run(registry);
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::{Command, PanicReport, format_panic_report, parse};

    const SECRET: &str = "hunter2-Sup3rS3cret";

    #[test]
    fn release_panic_report_never_contains_the_payload() {
        let location = std::panic::Location::caller();
        let payloads: [Box<dyn std::any::Any + Send>; 2] = [
            Box::new("password=hunter2-Sup3rS3cret"),
            Box::new(format!("connect failed: {SECRET}")),
        ];
        for payload in &payloads {
            let report = format_panic_report(PanicReport::Release, Some(location), &**payload);
            assert!(!report.contains(SECRET), "{report}");
            assert!(!report.contains("password"), "{report}");
            let at = format!(
                "{}:{}:{}",
                location.file(),
                location.line(),
                location.column()
            );
            assert!(report.contains(&at), "{report}");
        }
        let report = format_panic_report(PanicReport::Release, None, &SECRET);
        assert!(!report.contains(SECRET) && report.contains("unknown location"));
    }

    #[cfg(debug_assertions)]
    #[test]
    fn debug_panic_report_keeps_the_message() {
        let payload: Box<dyn std::any::Any + Send> = Box::new(String::from("boom 42"));
        let report = format_panic_report(PanicReport::Debug, None, &*payload);
        assert!(report.contains("boom 42"), "{report}");
        let report = format_panic_report(PanicReport::Debug, None, &7_u32);
        assert!(report.contains("non-string"), "{report}");
    }

    #[test]
    fn this_build_reports_payloads_only_with_debug_assertions() {
        assert_eq!(
            PanicReport::for_this_build() == PanicReport::Release,
            !cfg!(debug_assertions)
        );
    }

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
