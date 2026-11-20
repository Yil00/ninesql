//! The application's `tracing` subscriber (ADR-0029 §3), installed by the `ninesql` binary
//! only: this module belongs to `src/main.rs`, not to the library, and library crates never
//! install a subscriber.
//!
//! - **Stderr, at `warn` by default.** One line per event, no ANSI colours. `info` and `debug`
//!   are opt-in, so a desktop session's journal receives only warnings and errors (§3.2).
//!   Like the file log, stderr is written by its own thread fed through a bounded channel
//!   (dropped and counted when full), so an undrained stderr pipe never blocks a thread that
//!   logs (I6); the panic hook and [`LogGuard`] give it a bounded time to drain at exit.
//! - **A file log, off by default.** `[logging] file_level` in `settings.toml` (or
//!   `NINESQL_LOG`, below) turns on `$XDG_STATE_HOME/ninesql/logs/ninesql.log`: the directory
//!   `0700`, the file `0600`, opened with `O_NOFOLLOW` and refused unless it is a regular file
//!   owned by the user; at most [`MAX_FILE_BYTES`] per file and [`KEPT_FILES`] files
//!   (`ninesql.log`, `.1`, `.2`). One background thread writes it, fed through a bounded
//!   channel: when the channel is full the event is dropped and counted, and the count is
//!   logged once the writer catches up, so no thread (the UI thread included) ever waits on
//!   the disk (§3.3, I6).
//! - **Filter.** `NINESQL_LOG` takes the `RUST_LOG` syntax and applies to both sinks for one
//!   run, turning the file log on at its level; `RUST_LOG` is ignored (§3.4).
//! - **`trace` clamped in release builds.** Every filter is capped at `debug` and a warning
//!   says so once when a filter asked for more; statement text lives at `trace` (§3.5).
//! - **Field values escaped.** Control characters (newlines, carriage returns, the escape
//!   character of ANSI sequences…), line and paragraph separators and bidirectional controls
//!   are written as `\n`, `\r`, `\t` or `\u{…}`, in the message and in every field, on both
//!   sinks, so a value cannot forge a log line or drive a terminal (§3.6).
//!
//! The panic hook of `main.rs` stays installed first; panics do not go through `tracing`.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::time::{Duration, Instant};

use ninesql_core::preferences::LogFileLevel;
use ninesql_core::storage::Paths;
use tracing::field::{Field, Visit};
use tracing_subscriber::field::RecordFields;
use tracing_subscriber::filter::{EnvFilter, LevelFilter};
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::writer::BoxMakeWriter;
use tracing_subscriber::fmt::{FormatFields, MakeWriter};
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::{Layer as _, Registry};

/// The filter variable (`RUST_LOG` syntax). `RUST_LOG` itself is never read.
pub const ENV_VAR: &str = "NINESQL_LOG";
/// Stderr filter when `NINESQL_LOG` is not set.
pub const DEFAULT_STDERR: &str = "warn";
/// Largest size of one log file before it is rotated.
pub const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
/// Files kept: `ninesql.log`, `ninesql.log.1`, `ninesql.log.2`.
pub const KEPT_FILES: usize = 3;
/// Events waiting for the file writer before new ones are dropped.
pub const CHANNEL_CAPACITY: usize = 4096;
/// How long [`LogGuard`] waits for the writer to drain at exit.
const FLUSH_WAIT: Duration = Duration::from_secs(2);

/// What the subscriber does: the directives of each sink and whether `trace` is clamped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// Directives of the stderr sink.
    pub stderr: String,
    /// Directives of the file sink; `None`: no file log.
    pub file: Option<String>,
    /// Cap every filter at `debug` (release builds).
    pub clamp: bool,
}

/// Whether this build clamps `trace` (release builds, §3.5).
#[must_use]
pub const fn clamps_trace() -> bool {
    !cfg!(debug_assertions)
}

impl Plan {
    /// The plan for `env` (the value of `NINESQL_LOG`, if set) and the `[logging] file_level`
    /// of `settings.toml`. A blank `NINESQL_LOG` counts as unset.
    #[must_use]
    pub fn new(env: Option<&str>, file_level: LogFileLevel, clamp: bool) -> Self {
        match env.map(str::trim).filter(|v| !v.is_empty()) {
            Some(directives) => Self {
                stderr: directives.to_owned(),
                file: Some(directives.to_owned()),
                clamp,
            },
            None => Self {
                stderr: DEFAULT_STDERR.to_owned(),
                file: file_level.is_on().then(|| file_level.as_str().to_owned()),
                clamp,
            },
        }
    }
}

/// Parses `directives`; an invalid filter falls back to `fallback` and says why (the text of
/// the directive is not echoed: it came from the environment).
fn filter(directives: &str, fallback: &str, problems: &mut Vec<String>) -> EnvFilter {
    match EnvFilter::try_new(directives) {
        Ok(f) => f,
        Err(_) => {
            problems.push(format!(
                "{ENV_VAR} is not a valid filter; using \"{fallback}\""
            ));
            EnvFilter::new(fallback)
        }
    }
}

/// Whether `f` may enable events more verbose than `debug`.
fn asks_for_trace(f: &EnvFilter) -> bool {
    f.max_level_hint().is_none_or(|l| l > LevelFilter::DEBUG)
}

// ---------------------------------------------------------------------------------------------
// Field escaping (§3.6).

/// `value` with control characters, line / paragraph separators and bidirectional controls
/// escaped, so it stays on one line and cannot carry a terminal escape sequence.
#[must_use]
pub fn escape(value: &str) -> std::borrow::Cow<'_, str> {
    fn needs(c: char) -> bool {
        c.is_control()
            || matches!(
                c,
                '\u{2028}' | '\u{2029}' | '\u{200e}' | '\u{200f}' | '\u{061c}'
            )
            || ('\u{202a}'..='\u{202e}').contains(&c)
            || ('\u{2066}'..='\u{2069}').contains(&c)
    }
    if !value.chars().any(needs) {
        return std::borrow::Cow::Borrowed(value);
    }
    let mut out = String::with_capacity(value.len() + 8);
    for c in value.chars() {
        match c {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if needs(c) => out.push_str(&format!("\\u{{{:x}}}", u32::from(c))),
            c => out.push(c),
        }
    }
    std::borrow::Cow::Owned(out)
}

/// The field formatter of both sinks: `message` first as it is, then `name=value` pairs, every
/// value escaped ([`escape`]).
#[derive(Debug, Default, Clone, Copy)]
pub struct EscapedFields;

struct EscapingVisitor<'a> {
    writer: Writer<'a>,
    first: bool,
    result: std::fmt::Result,
}

impl EscapingVisitor<'_> {
    fn write(&mut self, field: &Field, value: &str) {
        if self.result.is_err() {
            return;
        }
        let sep = if self.first { "" } else { " " };
        self.first = false;
        let value = escape(value);
        self.result = if field.name() == "message" {
            write!(self.writer, "{sep}{value}")
        } else {
            write!(self.writer, "{sep}{}={value}", field.name())
        };
    }
}

impl Visit for EscapingVisitor<'_> {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.write(field, value);
        } else {
            self.write(field, &format!("{value:?}"));
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.write(field, &format!("{value:?}"));
    }
}

impl<'writer> FormatFields<'writer> for EscapedFields {
    fn format_fields<R: RecordFields>(
        &self,
        writer: Writer<'writer>,
        fields: R,
    ) -> std::fmt::Result {
        let mut visitor = EscapingVisitor {
            writer,
            first: true,
            result: Ok(()),
        };
        fields.record(&mut visitor);
        visitor.result
    }
}

// ---------------------------------------------------------------------------------------------
// The sinks (§3.3): the file log and stderr, each behind its own writer thread.

enum Msg {
    Line(Vec<u8>),
    Flush(SyncSender<()>),
}

/// Where a writer thread puts its lines.
trait Target: Send + 'static {
    /// Writes one formatted event; a failure loses the line, never blocks a caller.
    fn write_line(&mut self, bytes: &[u8]);
    /// Pushes buffered bytes out.
    fn flush(&mut self);
    /// The line reporting `lost` dropped events.
    fn dropped_line(&self, lost: u64) -> String;
}

/// One sink's writer side: formatted events go into a bounded channel, never to the disk or
/// to stderr on the caller's thread. When the channel is full the event is dropped and
/// counted; the writer thread reports the count at `warn` once it catches up. Cheap to clone.
#[derive(Clone)]
pub struct ChannelSink {
    tx: SyncSender<Msg>,
    dropped: Arc<AtomicU64>,
}

/// The file log's sink.
pub type FileSink = ChannelSink;

impl ChannelSink {
    /// A sink whose receiving end is returned instead of given to a writer thread (tests).
    fn detached(capacity: usize) -> (Self, Receiver<Msg>) {
        let (tx, rx) = mpsc::sync_channel(capacity);
        (
            Self {
                tx,
                dropped: Arc::new(AtomicU64::new(0)),
            },
            rx,
        )
    }

    /// A sink feeding `target` from a new thread named `name`.
    fn spawn<T: Target>(target: T, capacity: usize, name: &str) -> io::Result<Self> {
        let (sink, rx) = Self::detached(capacity);
        let dropped = Arc::clone(&sink.dropped);
        std::thread::Builder::new()
            .name(name.into())
            .spawn(move || writer_loop(target, &rx, &dropped))?;
        Ok(sink)
    }

    /// Opens the log in `dir` (`…/ninesql/logs`) and starts its writer thread. **Blocking**
    /// (a few file system calls): `main` setup only.
    ///
    /// # Errors
    /// The directory or the file cannot be made private, the file is not a regular file owned
    /// by the user (a symbolic link included), or the thread cannot start.
    pub fn open(dir: &Path, capacity: usize) -> io::Result<Self> {
        let log = LogFile::open(dir.to_path_buf())?;
        Self::spawn(log, capacity, "ninesql-log-writer")
    }

    /// A sink writing to `out` (the process's stderr in production) from its own thread, so
    /// an undrained stderr pipe never blocks a thread that logs (the UI thread included, I6).
    ///
    /// # Errors
    /// The thread cannot start.
    pub fn stream<W: Write + Send + 'static>(out: W, capacity: usize) -> io::Result<Self> {
        Self::spawn(StreamTarget(out), capacity, "ninesql-log-stderr")
    }

    /// Events dropped and not reported yet.
    #[cfg(test)]
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Waits (at most `wait`, the enqueueing included) until the writer has written
    /// everything queued before this call. Never blocks longer, even when the writer is stuck
    /// and the channel full.
    pub fn flush(&self, wait: Duration) {
        let deadline = Instant::now() + wait;
        let (tx, rx) = mpsc::sync_channel(1);
        let mut msg = Msg::Flush(tx);
        loop {
            match self.tx.try_send(msg) {
                Ok(()) => break,
                Err(TrySendError::Disconnected(_)) => return,
                Err(TrySendError::Full(back)) => {
                    if Instant::now() >= deadline {
                        return;
                    }
                    msg = back;
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        }
        let _ = rx.recv_timeout(deadline.saturating_duration_since(Instant::now()));
    }
}

/// One event's bytes for a [`ChannelSink`].
pub struct SinkWriter(ChannelSink);

impl Write for SinkWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // The fmt layer writes each formatted event with one `write_all`.
        match self.0.tx.try_send(Msg::Line(buf.to_vec())) {
            Ok(()) => {}
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                self.0.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for ChannelSink {
    type Writer = SinkWriter;

    fn make_writer(&'a self) -> Self::Writer {
        SinkWriter(self.clone())
    }
}

/// A byte stream (stderr) as a writer thread's target.
struct StreamTarget<W>(W);

impl<W: Write + Send + 'static> Target for StreamTarget<W> {
    fn write_line(&mut self, bytes: &[u8]) {
        let _ = self.0.write_all(bytes);
    }

    fn flush(&mut self) {
        let _ = self.0.flush();
    }

    fn dropped_line(&self, lost: u64) -> String {
        format!(
            "WARN ninesql::logging: {lost} log events were dropped (stderr was not being read)\n"
        )
    }
}

/// The open log file and its size.
struct LogFile {
    dir: PathBuf,
    file: File,
    size: u64,
}

fn file_name(n: usize) -> String {
    if n == 0 {
        "ninesql.log".to_owned()
    } else {
        format!("ninesql.log.{n}")
    }
}

impl LogFile {
    /// Makes `dir` (and its parent, the NineSQL state directory) private, opens the log and
    /// rotates it at once when it is over the cap.
    fn open(dir: PathBuf) -> io::Result<Self> {
        let storage = |e: ninesql_core::storage::StorageError| io::Error::other(e.to_string());
        if let Some(parent) = dir.parent() {
            ninesql_core::storage::fs::ensure_private_dir(parent).map_err(storage)?;
        }
        ninesql_core::storage::fs::ensure_private_dir(&dir).map_err(storage)?;
        let mut log = Self::open_file(dir)?;
        if log.size >= MAX_FILE_BYTES {
            log.rotate()?;
        }
        Ok(log)
    }

    fn open_file(dir: PathBuf) -> io::Result<Self> {
        let path = dir.join(file_name(0));
        let file = OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open(&path)
            .map_err(|e| {
                if e.raw_os_error() == Some(libc::ELOOP) {
                    io::Error::other("the log file is a symbolic link; it is refused")
                } else {
                    e
                }
            })?;
        let meta = file.metadata()?;
        if !meta.file_type().is_file() {
            return Err(io::Error::other("the log file is not a regular file"));
        }
        if meta.uid() != ninesql_core::storage::fs::current_uid() {
            return Err(io::Error::other(
                "the log file is not owned by the current user",
            ));
        }
        if meta.permissions().mode() & 0o077 != 0 {
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(Self {
            dir,
            size: meta.len(),
            file,
        })
    }

    /// `ninesql.log` → `.1` → `.2`, the old `.2` deleted, a new `ninesql.log` opened.
    fn rotate(&mut self) -> io::Result<()> {
        let last = self.dir.join(file_name(KEPT_FILES - 1));
        match std::fs::remove_file(&last) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        for n in (0..KEPT_FILES - 1).rev() {
            match std::fs::rename(self.dir.join(file_name(n)), self.dir.join(file_name(n + 1))) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        *self = Self::open_file(self.dir.clone())?;
        Ok(())
    }

    fn append(&mut self, bytes: &[u8]) {
        if self.size > 0
            && self.size + bytes.len() as u64 > MAX_FILE_BYTES
            && self.rotate().is_err()
        {
            // The log stops rather than growing past its cap.
            return;
        }
        if self.file.write_all(bytes).is_ok() {
            self.size += bytes.len() as u64;
        }
    }
}

impl Target for LogFile {
    fn write_line(&mut self, bytes: &[u8]) {
        self.append(bytes);
    }

    fn flush(&mut self) {
        let _ = self.file.flush();
    }

    fn dropped_line(&self, lost: u64) -> String {
        format!(
            "WARN ninesql::logging: {lost} log events were dropped (the log writer was behind)\n"
        )
    }
}

fn writer_loop<T: Target>(mut target: T, rx: &Receiver<Msg>, dropped: &AtomicU64) {
    while let Ok(msg) = rx.recv() {
        match msg {
            Msg::Line(bytes) => {
                target.write_line(&bytes);
                let lost = dropped.swap(0, Ordering::Relaxed);
                if lost > 0 {
                    let line = target.dropped_line(lost);
                    target.write_line(line.as_bytes());
                }
            }
            Msg::Flush(done) => {
                target.flush();
                let _ = done.send(());
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Assembly.

/// The installed sinks, for [`flush_before_exit`] (set once by [`install`]).
static INSTALLED: OnceLock<Sinks> = OnceLock::new();

/// The writer-thread sinks of the process's subscriber.
#[derive(Clone, Default)]
struct Sinks {
    stderr: Option<ChannelSink>,
    file: Option<FileSink>,
}

impl Sinks {
    /// Flushes both sinks within `wait` in all.
    fn flush(&self, wait: Duration) {
        let deadline = Instant::now() + wait;
        for sink in [&self.stderr, &self.file].into_iter().flatten() {
            sink.flush(deadline.saturating_duration_since(Instant::now()));
        }
    }
}

/// Best effort, at most `wait`: lets the writer threads write the events queued so far (the
/// panic hook calls it before it prints its report, so the last lines come first). Does
/// nothing before [`install`].
pub fn flush_before_exit(wait: Duration) {
    if let Some(sinks) = INSTALLED.get() {
        sinks.flush(wait);
    }
}

/// What [`install`] leaves running: the stderr and file sinks, flushed (within
/// [`FLUSH_WAIT`] in all) when this is dropped.
pub struct LogGuard {
    sinks: Sinks,
}

impl Drop for LogGuard {
    fn drop(&mut self) {
        self.sinks.flush(FLUSH_WAIT);
    }
}

/// The subscriber of `plan`, writing stderr lines to `stderr` and the file log to `file`.
/// Returns it with the problems to report once it is installed.
pub fn build<W>(
    plan: &Plan,
    stderr: W,
    file: Option<FileSink>,
) -> (
    impl tracing::Subscriber + Send + Sync + 'static,
    Vec<String>,
)
where
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    let mut problems = Vec::new();
    let stderr_filter = filter(&plan.stderr, DEFAULT_STDERR, &mut problems);
    let file_filter = plan
        .file
        .as_deref()
        .map(|d| filter(d, DEFAULT_STDERR, &mut problems));
    if plan.clamp
        && (asks_for_trace(&stderr_filter) || file_filter.as_ref().is_some_and(asks_for_trace))
    {
        problems.push(
            "trace logging is not available in release builds: the log filter is capped at debug"
                .to_owned(),
        );
    }
    let clamp = plan.clamp.then_some(LevelFilter::DEBUG);
    let stderr_layer = tracing_subscriber::fmt::layer()
        .fmt_fields(EscapedFields)
        .with_writer(stderr)
        .with_filter(stderr_filter);
    let file_layer = match (file, file_filter) {
        (Some(sink), Some(f)) => Some(
            tracing_subscriber::fmt::layer()
                .fmt_fields(EscapedFields)
                .with_writer(sink)
                .with_filter(f),
        ),
        _ => None,
    };
    let subscriber = Registry::default()
        .with(clamp)
        .with(stderr_layer)
        .with(file_layer);
    (subscriber, problems)
}

/// Installs the process's subscriber: reads `NINESQL_LOG` and the `[logging] file_level` of the
/// settings under `paths`, opens the file log when asked. **Blocking** (`settings.toml` and the
/// log file): call it from `main` before GPUI starts. Problems are logged once installed.
pub fn install(paths: Option<&Paths>) -> LogGuard {
    let mut problems = Vec::new();
    let file_level = match paths {
        Some(p) => match ninesql_core::preferences::Preferences::load(&p.settings_file()) {
            Ok((prefs, _)) => prefs.logging.file_level,
            // The UI reports settings problems itself.
            Err(_) => LogFileLevel::Off,
        },
        None => LogFileLevel::Off,
    };
    let env = std::env::var(ENV_VAR).ok();
    let plan = Plan::new(env.as_deref(), file_level, clamps_trace());
    let file = match (&plan.file, paths) {
        (Some(_), Some(p)) => match FileSink::open(&log_dir(p), CHANNEL_CAPACITY) {
            Ok(sink) => Some(sink),
            Err(e) => {
                // The kind and the directory's last component only: the error's text can
                // carry the full path, which names the user's home (§3.2).
                problems.push(format!(
                    "the file log cannot be written in {}: {}",
                    ninesql_core::storage::fs::log_name(&log_dir(p)),
                    e.kind()
                ));
                None
            }
        },
        (Some(_), None) => {
            problems.push("the file log has no location (no home directory)".to_owned());
            None
        }
        _ => None,
    };
    // Stderr goes through its own writer thread: an undrained stderr pipe (a stopped pager, a
    // journal that is behind) never blocks a thread that logs, the UI thread included (I6).
    // Without that thread, stderr gets no events rather than blocking ones.
    let stderr = ChannelSink::stream(io::stderr(), CHANNEL_CAPACITY).ok();
    let stderr_writer = match &stderr {
        Some(sink) => BoxMakeWriter::new(sink.clone()),
        None => BoxMakeWriter::new(io::sink),
    };
    let (subscriber, mut more) = build(&plan, stderr_writer, file.clone());
    problems.append(&mut more);
    let sinks = Sinks { stderr, file };
    if tracing::subscriber::set_global_default(subscriber).is_ok() {
        let _ = INSTALLED.set(sinks.clone());
        for p in &problems {
            tracing::warn!(problem = %p, "logging");
        }
    }
    LogGuard { sinks }
}

/// `$XDG_STATE_HOME/ninesql/logs`.
#[must_use]
pub fn log_dir(paths: &Paths) -> PathBuf {
    paths.state_dir().join("logs")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A stderr stand-in: everything written, shared.
    #[derive(Clone, Default)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl Write for Buffer {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for Buffer {
        type Writer = Self;
        fn make_writer(&'a self) -> Self {
            self.clone()
        }
    }

    impl Buffer {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    fn private_dir() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("ninesql").join("logs");
        (tmp, dir)
    }

    fn read_log(dir: &Path) -> String {
        std::fs::read_to_string(dir.join("ninesql.log")).unwrap_or_default()
    }

    #[test]
    fn plan_defaults_env_and_file_level() {
        let p = Plan::new(None, LogFileLevel::Off, false);
        assert_eq!(p.stderr, "warn");
        assert_eq!(p.file, None);
        let p = Plan::new(Some("  "), LogFileLevel::Info, true);
        assert_eq!(
            (p.stderr.as_str(), p.file.as_deref()),
            ("warn", Some("info"))
        );
        let p = Plan::new(Some("ninesql_core=debug,warn"), LogFileLevel::Off, false);
        assert_eq!(p.stderr, "ninesql_core=debug,warn");
        assert_eq!(
            p.file.as_deref(),
            Some("ninesql_core=debug,warn"),
            "env turns the file on"
        );
    }

    #[test]
    fn rust_log_is_never_read() {
        let source = include_str!("logging.rs");
        let needle = ["RUST", "_LOG"].concat();
        let reads = source
            .lines()
            .filter(|l| l.contains(&needle) && l.contains("env::var"))
            .count();
        assert_eq!(reads, 0);
    }

    #[test]
    fn release_clamp_holds_at_debug() {
        let out = Buffer::default();
        let plan = Plan::new(Some("trace"), LogFileLevel::Off, true);
        let (subscriber, problems) = build(&plan, out.clone(), None);
        assert!(
            problems.iter().any(|p| p.contains("capped at debug")),
            "{problems:?}"
        );
        tracing::subscriber::with_default(subscriber, || {
            tracing::trace!("trace-probe-line");
            tracing::debug!("debug-probe-line");
        });
        let text = out.text();
        assert!(!text.contains("trace-probe-line"), "{text}");
        assert!(text.contains("debug-probe-line"), "{text}");

        // A target-only directive means every level: clamped too.
        let out = Buffer::default();
        let plan = Plan::new(Some("ninesql"), LogFileLevel::Off, true);
        let (subscriber, _) = build(&plan, out.clone(), None);
        tracing::subscriber::with_default(subscriber, || {
            tracing::trace!(target: "ninesql::x", "trace-probe-line");
        });
        assert!(!out.text().contains("trace-probe-line"));

        // Without the clamp (debug builds), trace passes and nothing is said.
        let out = Buffer::default();
        let plan = Plan::new(Some("trace"), LogFileLevel::Off, false);
        let (subscriber, problems) = build(&plan, out.clone(), None);
        assert!(problems.is_empty());
        tracing::subscriber::with_default(subscriber, || tracing::trace!("trace-probe-line"));
        assert!(out.text().contains("trace-probe-line"));
        assert_eq!(clamps_trace(), !cfg!(debug_assertions));
    }

    #[test]
    fn stderr_defaults_to_warn_and_a_bad_filter_falls_back() {
        let out = Buffer::default();
        let (subscriber, problems) =
            build(&Plan::new(None, LogFileLevel::Off, true), out.clone(), None);
        assert!(problems.is_empty());
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("info-line");
            tracing::warn!("warn-line");
        });
        let text = out.text();
        assert!(
            !text.contains("info-line") && text.contains("warn-line"),
            "{text}"
        );
        assert!(!text.contains('\u{1b}'), "no ANSI colours");

        let out = Buffer::default();
        let (_, problems) = build(
            &Plan::new(Some("[[[=x\u{1b}"), LogFileLevel::Off, false),
            out,
            None,
        );
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems.iter().all(|p| !p.contains('\u{1b}')), "not echoed");
    }

    #[test]
    fn escape_keeps_values_on_one_line() {
        assert_eq!(escape("plain value"), "plain value");
        assert_eq!(escape("a\nb\r\tc"), "a\\nb\\r\\tc");
        assert_eq!(escape("\u{1b}[31mred"), "\\u{1b}[31mred");
        assert_eq!(
            escape("x\u{2028}y\u{202e}z\u{7f}"),
            "x\\u{2028}y\\u{202e}z\\u{7f}"
        );
    }

    #[test]
    fn newlines_and_ansi_in_fields_are_escaped_on_both_sinks() {
        let (_tmp, dir) = private_dir();
        let sink = FileSink::open(&dir, 64).unwrap();
        let out = Buffer::default();
        let plan = Plan::new(Some("info"), LogFileLevel::Off, false);
        let (subscriber, _) = build(&plan, out.clone(), Some(sink.clone()));
        let forged = "x\nFAKE ERROR ninesql: forged line\u{1b}[2J\u{1b}[31m";
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!(value = %forged, other = forged, "msg {}", forged);
        });
        sink.flush(Duration::from_secs(5));
        for text in [out.text(), read_log(&dir)] {
            assert_eq!(text.lines().count(), 1, "{text}");
            assert!(!text.contains('\u{1b}'), "{text}");
            assert!(text.contains("\\nFAKE ERROR"), "{text}");
            assert!(text.contains("\\u{1b}[2J"), "{text}");
        }
    }

    #[test]
    fn file_is_private_in_a_private_directory_and_refuses_a_link() {
        let (tmp, dir) = private_dir();
        let sink = FileSink::open(&dir, 64).unwrap();
        sink.flush(Duration::from_secs(5));
        let mode = |p: &Path| std::fs::symlink_metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(dir.parent().unwrap()), 0o700);
        assert_eq!(mode(&dir.join("ninesql.log")), 0o600);

        // A symbolic link in place of the file is refused, never followed.
        let other = tempfile::tempdir().unwrap();
        let dir2 = tmp.path().join("b").join("logs");
        std::fs::create_dir_all(&dir2).unwrap();
        let target = other.path().join("elsewhere");
        std::os::unix::fs::symlink(&target, dir2.join("ninesql.log")).unwrap();
        assert!(FileSink::open(&dir2, 64).is_err());
        assert!(!target.exists(), "the link's target was not created");
        // A directory in place of the log directory that is a link: refused too.
        let dir3 = tmp.path().join("c");
        std::fs::create_dir_all(&dir3).unwrap();
        std::os::unix::fs::symlink(other.path(), dir3.join("logs")).unwrap();
        assert!(FileSink::open(&dir3.join("logs"), 64).is_err());
        // A file with group / other bits is tightened.
        let dir4 = tmp.path().join("d").join("logs");
        std::fs::create_dir_all(&dir4).unwrap();
        std::fs::write(dir4.join("ninesql.log"), b"old\n").unwrap();
        std::fs::set_permissions(
            dir4.join("ninesql.log"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        let _ = FileSink::open(&dir4, 64).unwrap();
        assert_eq!(mode(&dir4.join("ninesql.log")), 0o600);
    }

    #[test]
    fn rotation_keeps_at_most_three_files() {
        let (_tmp, dir) = private_dir();
        std::fs::create_dir_all(&dir).unwrap();
        // Over the cap at startup: rotated at once.
        std::fs::write(dir.join("ninesql.log"), vec![b'a'; MAX_FILE_BYTES as usize]).unwrap();
        let mut log = LogFile::open(dir.clone()).unwrap();
        assert_eq!(log.size, 0);
        assert!(dir.join("ninesql.log.1").exists());
        let line = vec![b'x'; 1024 * 1024];
        for _ in 0..40 {
            log.append(&line);
        }
        let mut names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["ninesql.log", "ninesql.log.1", "ninesql.log.2"]);
        for n in &names {
            let len = std::fs::metadata(dir.join(n)).unwrap().len();
            assert!(len <= MAX_FILE_BYTES, "{n}: {len}");
        }
    }

    #[test]
    fn a_full_channel_drops_instead_of_blocking() {
        // No writer thread: the channel fills after one event.
        let (sink, rx) = FileSink::detached(1);
        let (subscriber, _) = build(
            &Plan::new(Some("info"), LogFileLevel::Off, false),
            io::sink,
            Some(sink.clone()),
        );
        let started = std::time::Instant::now();
        tracing::subscriber::with_default(subscriber, || {
            for i in 0..100 {
                tracing::info!(i, "event");
            }
        });
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(sink.dropped(), 99);
        drop(rx);

        // With a writer: the drop count is logged once the writer catches up.
        let (_tmp, dir) = private_dir();
        let log = LogFile::open(dir.clone()).unwrap();
        let (sink, rx) = FileSink::detached(2);
        let mut w = sink.make_writer();
        for _ in 0..5 {
            w.write_all(b"line\n").unwrap();
        }
        assert_eq!(sink.dropped(), 3);
        let dropped = Arc::clone(&sink.dropped);
        let t = std::thread::spawn(move || writer_loop(log, &rx, &dropped));
        sink.flush(Duration::from_secs(5));
        drop(sink);
        drop(w);
        t.join().unwrap();
        let text = read_log(&dir);
        assert!(text.contains("3 log events were dropped"), "{text}");
    }

    /// A stderr stand-in whose first write waits until the test opens the gate: an undrained
    /// pipe.
    struct Stuck {
        gate: Arc<std::sync::Barrier>,
        opened: bool,
        out: Buffer,
    }

    impl Write for Stuck {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if !self.opened {
                self.gate.wait();
                self.opened = true;
            }
            self.out.write(buf)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_stuck_stderr_never_blocks_warn_callers() {
        let gate = Arc::new(std::sync::Barrier::new(2));
        let out = Buffer::default();
        let stuck = Stuck {
            gate: Arc::clone(&gate),
            opened: false,
            out: out.clone(),
        };
        let sink = ChannelSink::stream(stuck, 4).unwrap();
        let (subscriber, _) = build(
            &Plan::new(None, LogFileLevel::Off, false),
            sink.clone(),
            None,
        );
        let started = Instant::now();
        tracing::subscriber::with_default(subscriber, || {
            for i in 0..200 {
                tracing::warn!(i, "stuck-probe");
            }
        });
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "warn! waited on stderr: {:?}",
            started.elapsed()
        );
        // A flush while the writer is stuck and the channel full returns within its bound.
        let started = Instant::now();
        sink.flush(Duration::from_millis(100));
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(sink.dropped() > 0, "a full channel drops");

        // Once stderr drains, the queued lines and the drop count come out.
        gate.wait();
        sink.flush(Duration::from_secs(5));
        let text = out.text();
        assert!(text.contains("stuck-probe"), "{text}");
        assert!(
            text.contains("log events were dropped (stderr was not being read)"),
            "{text}"
        );
        assert!(text.lines().all(|l| !l.is_empty()));
    }

    #[test]
    fn flush_before_exit_without_install_returns_at_once() {
        let started = Instant::now();
        flush_before_exit(Duration::from_secs(5));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    /// I1 (ADR-0029 §3.7): a connect with a password, a statement with a credential word and a
    /// statement on a production connection, with the file log at `debug`: neither the password
    /// nor the statement text reaches the log. The subscriber is the process's global one, so
    /// the core's runtime threads log into it.
    #[cfg(not(feature = "e2e-scenarios"))]
    #[test]
    fn file_log_at_debug_holds_no_password_nor_statement_text() {
        use ninesql_core::driver_api::mock::MockDriver;
        use ninesql_core::secrets::{
            MemoryStore, SecretId, SecretKind, SecretMeta, SecretRef, SecretStores, SecretString,
            StoreKind,
        };
        use ninesql_core::{
            Auth, Connection, ConnectionTypeId, ConnectionTypes, DriverRegistry, Endpoint,
            ExecutionService, KnownHostsFiles, ManagerConfig, OpenOutcome, OpenRequest, RunEvent,
            RunRequest, SecretSource, SessionManager, TlsMode,
        };

        const PASSWORD: &str = "pw-probe-6f1c2a9e";
        const CREDENTIAL_STATEMENT: &str =
            "ALTER USER probe_user PASSWORD 'stmt-probe-credential-71b3'";
        const PRODUCTION_STATEMENT: &str = "DELETE FROM stmt_probe_production_table_93ad";

        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("ninesql").join("logs");
        let sink = FileSink::open(&dir, CHANNEL_CAPACITY).unwrap();
        let plan = Plan::new(None, LogFileLevel::Debug, clamps_trace());
        let (subscriber, _) = build(&plan, io::sink, Some(sink.clone()));
        tracing::subscriber::set_global_default(subscriber).unwrap();

        let mut registry = DriverRegistry::new();
        registry
            .register(Arc::new(MockDriver::new().with_capabilities(
                ninesql_core::driver_api::mock::full_capabilities(),
            )));
        let store = Arc::new(MemoryStore::new(StoreKind::Vault));
        let mut stores = SecretStores::new();
        stores.register(store);
        let mut config = ManagerConfig::with_known_hosts(KnownHostsFiles {
            own: tmp.path().join("known_hosts"),
            user: None,
        });
        config.workers = 2;
        let manager = SessionManager::new(config, registry, stores.clone()).unwrap();
        let exec = ExecutionService::new(manager.clone(), None, Default::default());
        let types = ConnectionTypes::default();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        for ty in [ConnectionTypeId::dev(), ConnectionTypeId::prod()] {
            let reference = SecretRef::new(
                StoreKind::Vault,
                SecretId::new(format!("probe:{ty}")).unwrap(),
            );
            let meta = SecretMeta {
                connection_id: "probe".into(),
                kind: SecretKind::Password,
                label: "probe".into(),
            };
            stores
                .save(&reference, &meta, &SecretString::from(PASSWORD.to_owned()))
                .unwrap();
            let mut c = Connection::new(
                "Probe",
                ninesql_core::EngineId::new("mock").unwrap(),
                Endpoint::Tcp {
                    host: "mock.example.test".into(),
                    port: Some(5000),
                    database: Some("app".into()),
                },
                Auth::Password {
                    username: "probe_user".into(),
                    secret: SecretSource::Stored(reference),
                },
                ty.clone(),
            );
            c.tls.mode = TlsMode::Disable;
            let outcome = rt
                .block_on(manager.open(OpenRequest::new(c, &types)))
                .expect("an answer")
                .expect("opened");
            let OpenOutcome::Opened(handle) = outcome else {
                panic!("not opened");
            };
            for sql in [CREDENTIAL_STATEMENT, PRODUCTION_STATEMENT] {
                let mut run = exec.run(&handle, RunRequest::statement(sql));
                rt.block_on(async {
                    while let Ok(Some(e)) =
                        tokio::time::timeout(Duration::from_secs(10), run.next()).await
                    {
                        if matches!(e, RunEvent::Done(_)) {
                            break;
                        }
                    }
                });
            }
            let _ = rt.block_on(handle.close());
        }
        tracing::debug!("i1-probe-end-marker");
        sink.flush(Duration::from_secs(5));
        let text = read_log(&dir);
        assert!(text.contains("i1-probe-end-marker"), "the log was written");
        assert!(
            text.lines().any(|l| l.contains("ninesql_core")),
            "the core's runtime threads log into the file"
        );
        for probe in [
            PASSWORD,
            "stmt-probe-credential-71b3",
            "stmt_probe_production_table_93ad",
        ] {
            assert!(
                !text.contains(probe),
                "I1: a probe value reached the file log"
            );
        }
    }
}
