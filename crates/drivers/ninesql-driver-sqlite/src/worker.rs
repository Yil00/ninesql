//! The connection thread: one dedicated OS thread per session owns the `rusqlite::Connection`
//! (SQLite is synchronous). The async side sends [`Command`]s; results travel back through
//! oneshot channels, one batch per demand of the result stream, so nothing runs ahead of the
//! consumer (I6). A dedicated thread, rather than the shared blocking pool, because a paused
//! result keeps its statement (and the thread) waiting for the next demand.
//!
//! # Engine-side protections (docs/08)
//! Every connection, read-only or not: `SQLITE_OPEN_NOFOLLOW` on a canonical absolute path (no
//! URI interpretation, no symbolic link swapped in after the check), `SQLITE_DBCONFIG_DEFENSIVE`
//! (no `writable_schema`, no `journal_mode = OFF`, no shadow-table writes),
//! `SQLITE_DBCONFIG_TRUSTED_SCHEMA` off (views and triggers of a hostile file cannot call
//! functions with side effects), `cell_size_check`, `load_extension` never enabled and denied by
//! the authorizer, `SQLITE_LIMIT_LENGTH` tied to the caller's byte budget.
//!
//! Read-only sessions add `SQLITE_OPEN_READ_ONLY`, `PRAGMA query_only = ON`,
//! `SQLITE_LIMIT_ATTACHED = 0`, `SQLITE_DBCONFIG_ENABLE_ATTACH_CREATE` / `_WRITE` off and an
//! authorizer that allows only reads (`SELECT`, reads of tables, recursive CTEs, transaction
//! control and an allowlist of schema-reading pragmas): every write, `ATTACH` (and so
//! `VACUUM INTO`, which attaches its target), `DETACH`, `ANALYZE`, `REINDEX`, DDL on temporary
//! objects and every other pragma are denied while the statement is prepared. The authorizer
//! and the open mode cannot be changed from SQL; after every statement the driver still checks
//! that the file is read-only, that nothing is attached, that `query_only` is on and the
//! connection limits and flags are intact, re-asserts `query_only`, and closes the session if
//! anything changed.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use ninesql_driver_api::{
    BatchLimiter, ColumnInfo, Dialect, DriverError, DriverResult, ExecutionGuard, FetchBudget,
    ForeignKeyInfo, IndexInfo, Notice, NoticeSeverity, ObjectDetails, ObjectInfo, ObjectKind,
    ObjectPath, Page, PageRequest, Param, ReferentialAction, ResultItem, ResultSetHeader,
    RowBudget, StatementClass, TableInfo, TriggerInfo, escape_like, truncate_str,
};
use rusqlite::config::DbConfig;
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::limits::Limit;
use rusqlite::types::{ToSql, Value};
use rusqlite::{Connection, ErrorCode, InterruptHandle, MAIN_DB, OpenFlags};
use tokio::sync::oneshot;

use crate::dialect::{PRAGMAS_READ, PRAGMAS_WITH_ARG, SqliteDialect};
use crate::types::{self, MAX_COLUMN_NAME, Val};

/// A result left paused longer than this is closed by the driver: its statement holds a read
/// transaction (a `SHARED` lock or a WAL snapshot) that keeps writers of other processes
/// waiting.
pub const PAUSED_RESULT_TIMEOUT: Duration = Duration::from_secs(300);

/// Bound of the driver's own statements (setup, read-only checks, navigator, transaction
/// control). Enforced by the progress handler.
pub(crate) const INTERNAL_TIMEOUT: Duration = Duration::from_secs(60);

/// How often a thread waiting for the next demand checks the cancel and close flags.
const DEMAND_POLL: Duration = Duration::from_millis(50);

/// Virtual-machine instructions between two calls of the progress handler.
const PROGRESS_OPS: i32 = 1000;

/// Smallest and largest `SQLITE_LIMIT_LENGTH` set for a statement (bytes).
const MIN_LENGTH_LIMIT: u64 = 16 * 1024 * 1024;
const MAX_LENGTH_LIMIT: u64 = 1_000_000_000;

/// Largest DDL text returned by the navigator.
const MAX_DDL_BYTES: u64 = 1024 * 1024;

/// Functions denied by the authorizer in every session: extension loading (never enabled
/// either), the FTS3 tokenizer pointer interface and the file functions of the `sqlite3`
/// shell, should a build ever register them.
const DENIED_FUNCTIONS: &[&str] = &[
    "load_extension",
    "fts3_tokenizer",
    "readfile",
    "writefile",
    "edit",
];

/// Pragmas no session may set: they would lift a protection the driver applies to every
/// connection (`trusted_schema`, `cell_size_check`) or rewrite the schema (`writable_schema`,
/// also neutralised by the defensive flag).
const PROTECTION_PRAGMAS: &[&str] = &["trusted_schema", "cell_size_check", "writable_schema"];

/// Pragmas only the driver itself sets (while [`Worker::privileged`] is on).
const DRIVER_PRAGMAS: &[&str] = &[
    "query_only",
    "cache_size",
    "foreign_keys",
    "cell_size_check",
];

/// Session state shared by the session, its streams and the connection thread.
#[derive(Debug)]
pub(crate) struct Shared {
    pub(crate) read_only: bool,
    pub(crate) auto_commit: AtomicBool,
    pub(crate) in_tx: AtomicBool,
    pub(crate) closed: AtomicBool,
    /// Set by `close`: a thread waiting for a demand gives up.
    pub(crate) closing: AtomicBool,
    reason: Mutex<Option<String>>,
}

impl Shared {
    pub(crate) fn new(read_only: bool, auto_commit: bool) -> Self {
        Self {
            read_only,
            auto_commit: AtomicBool::new(auto_commit),
            in_tx: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            closing: AtomicBool::new(false),
            reason: Mutex::new(None),
        }
    }

    pub(crate) fn mark_closed(&self, reason: &str) {
        let mut r = self.reason.lock().unwrap_or_else(PoisonError::into_inner);
        if r.is_none() {
            *r = Some(reason.to_owned());
        }
        drop(r);
        self.closed.store(true, Ordering::SeqCst);
    }

    pub(crate) fn closed_error(&self) -> DriverError {
        let r = self.reason.lock().unwrap_or_else(PoisonError::into_inner);
        DriverError::Closed(r.clone().unwrap_or_else(|| "connection lost".to_owned()))
    }
}

/// State of one execution, shared by its stream, its cancel handle and the thread.
#[derive(Debug, Default)]
pub(crate) struct ExecState {
    pub(crate) cancelled: AtomicBool,
    /// The thread ended the execution. A mutex rather than an atomic: a stream dropped
    /// mid-execution interrupts the connection while holding it, so the interrupt can never
    /// land after the thread moved on to the next execution (review M2).
    pub(crate) ended: Mutex<bool>,
    /// Error to report to the stream when it asks again (a paused result that was closed).
    pub(crate) end_error: Mutex<Option<DriverError>>,
}

/// Items for one demand of the result stream.
pub(crate) struct Chunk {
    pub(crate) items: Vec<DriverResult<ResultItem>>,
    pub(crate) done: bool,
}

pub(crate) type DemandRx = Receiver<oneshot::Sender<Chunk>>;

pub(crate) struct ExecRequest {
    pub(crate) text: String,
    pub(crate) params: Vec<Param>,
    pub(crate) budget: FetchBudget,
    pub(crate) class: StatementClass,
    pub(crate) guard: ExecutionGuard,
    pub(crate) exec: Arc<ExecState>,
    pub(crate) demands: DemandRx,
}

/// Driver operations that are not user statements.
pub(crate) enum Op {
    /// Driver SQL (transaction control, savepoints with quoted names).
    Sql(String),
    Commit,
    Rollback,
    SetAutoCommit(bool),
    Ping,
    List {
        parent: ObjectPath,
        kind: ObjectKind,
        page: PageRequest,
    },
    Ddl(ObjectPath),
}

pub(crate) enum OpOut {
    Done,
    Page(Page<ObjectInfo>),
    Ddl(String),
}

pub(crate) enum Command {
    Execute(Box<ExecRequest>),
    Op {
        op: Op,
        guard: ExecutionGuard,
        reply: oneshot::Sender<DriverResult<OpOut>>,
    },
    Close {
        reply: oneshot::Sender<()>,
    },
}

/// What to open.
#[derive(Clone, Debug)]
pub(crate) enum Target {
    File { path: PathBuf, create: bool },
    Memory,
}

/// Settings applied at open.
#[derive(Clone, Debug)]
pub(crate) struct OpenOptions {
    pub(crate) read_only: bool,
    /// Open immutable (read-only sessions only): a file inside a DBeaver workspace (I4).
    pub(crate) immutable: bool,
    pub(crate) busy_timeout: Duration,
    /// `PRAGMA` statements built by the driver from validated property values.
    pub(crate) pragmas: Vec<String>,
}

/// What the async side needs once the file is open.
pub(crate) struct Opened {
    pub(crate) version: String,
    pub(crate) interrupt: Arc<InterruptHandle>,
}

/// Starts the connection thread. The receiver yields once the database is open and set up.
pub(crate) fn spawn(
    target: Target,
    options: OpenOptions,
    shared: Arc<Shared>,
) -> DriverResult<(Sender<Command>, oneshot::Receiver<DriverResult<Opened>>)> {
    let (tx, rx) = std::sync::mpsc::channel::<Command>();
    let (open_tx, open_rx) = oneshot::channel();
    std::thread::Builder::new()
        .name("ninesql-sqlite".to_owned())
        .spawn(move || {
            let outcome =
                catch_unwind(AssertUnwindSafe(|| Worker::open(&target, &options, shared)));
            let worker = match outcome {
                Ok(Ok(w)) => w,
                Ok(Err(e)) => {
                    let _ = open_tx.send(Err(e));
                    return;
                }
                Err(_) => {
                    let _ = open_tx.send(Err(DriverError::Connect(
                        "the SQLite client failed while opening the database".into(),
                    )));
                    return;
                }
            };
            let opened = Opened {
                version: rusqlite::version().to_owned(),
                interrupt: Arc::clone(&worker.interrupt),
            };
            if open_tx.send(Ok(opened)).is_err() {
                // The connect timed out meanwhile.
                worker.shutdown();
                return;
            }
            worker.run(&rx);
        })
        .map_err(|e| DriverError::Connect(format!("cannot start the SQLite thread: {e}")))?;
    Ok((tx, open_rx))
}

/// Resolves the database path: absolute only (a relative path would depend on the working
/// directory), canonical (symbolic links and `..` resolved, so the file opened is the file
/// checked, and SQLite never sees a `file:` URI or `:memory:`), a regular file (not a directory,
/// a FIFO or a device). A missing file is created only when asked and the session is
/// read-write, in an existing directory.
pub(crate) fn resolve_path(path: &Path, create: bool) -> DriverResult<PathBuf> {
    if path.as_os_str().is_empty() || !path.is_absolute() {
        return Err(DriverError::InvalidParams(
            "the SQLite database path must be an absolute file path".into(),
        ));
    }
    match std::fs::canonicalize(path) {
        Ok(real) => {
            let md = std::fs::metadata(&real).map_err(|e| {
                DriverError::Io(format!("cannot read the database file: {}", e.kind()))
            })?;
            if md.is_file() {
                Ok(real)
            } else {
                Err(DriverError::InvalidParams(
                    "the SQLite database path is not a regular file".into(),
                ))
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && create => {
            let name = match path.components().next_back() {
                Some(Component::Normal(n)) => n.to_owned(),
                _ => {
                    return Err(DriverError::InvalidParams(
                        "the SQLite database path must end with a file name".into(),
                    ));
                }
            };
            let parent = path.parent().ok_or_else(|| {
                DriverError::InvalidParams("the SQLite database path has no directory".into())
            })?;
            let dir = std::fs::canonicalize(parent).map_err(|e| {
                DriverError::Io(format!("cannot open the database directory: {}", e.kind()))
            })?;
            if !dir.is_dir() {
                return Err(DriverError::InvalidParams(
                    "the parent of the SQLite database path is not a directory".into(),
                ));
            }
            Ok(dir.join(name))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Err(DriverError::Io("the database file does not exist".into()))
        }
        Err(e) => Err(DriverError::Io(format!(
            "cannot open the database file: {}",
            e.kind()
        ))),
    }
}

/// `true` for a database file in WAL mode (header bytes 18 and 19 equal to 2) when neither its
/// `-wal` nor its `-shm` file exists (not even as a dangling link).
pub(crate) fn wal_without_sidecars(path: &Path) -> bool {
    use std::io::Read;
    let mut header = [0u8; 100];
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    if f.read_exact(&mut header).is_err()
        || &header[..16] != b"SQLite format 3\0"
        || header[18] != 2
        || header[19] != 2
    {
        return false;
    }
    !has_sidecar(path, "-wal") && !has_sidecar(path, "-shm")
}

/// `true` when `<path><suffix>` exists (even as a dangling link).
pub(crate) fn has_sidecar(path: &Path, suffix: &str) -> bool {
    let mut p = path.as_os_str().to_owned();
    p.push(suffix);
    std::fs::symlink_metadata(PathBuf::from(p)).is_ok()
}

/// `file:<canonical path>?mode=ro&immutable=1`, every byte outside `[A-Za-z0-9/._~-]`
/// percent-encoded, so no character of the path can add a URI parameter.
pub(crate) fn immutable_uri(path: &Path) -> String {
    use std::fmt::Write;
    use std::os::unix::ffi::OsStrExt;
    let mut uri = String::from("file:");
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'_' | b'~' | b'-') {
            uri.push(char::from(b));
        } else {
            let _ = write!(uri, "%{b:02X}");
        }
    }
    uri.push_str("?mode=ro&immutable=1");
    uri
}

/// The authorizer decision (pure, unit-tested).
pub(crate) fn authorize(
    read_only: bool,
    privileged: bool,
    action: &AuthAction<'_>,
) -> Authorization {
    if let AuthAction::Function { function_name } = action {
        return if DENIED_FUNCTIONS
            .iter()
            .any(|f| f.eq_ignore_ascii_case(function_name))
        {
            Authorization::Deny
        } else {
            Authorization::Allow
        };
    }
    // In every session: the pragmas that would undo the driver's protections against hostile
    // files (review L1). Reading them is allowed; the driver alone sets `cell_size_check`.
    if let AuthAction::Pragma {
        pragma_name,
        pragma_value: Some(_),
    } = action
    {
        let name = pragma_name.to_ascii_lowercase();
        if PROTECTION_PRAGMAS.contains(&name.as_str()) && !(privileged && name == "cell_size_check")
        {
            return Authorization::Deny;
        }
    }
    if !read_only {
        return Authorization::Allow;
    }
    match action {
        AuthAction::Select
        | AuthAction::Read { .. }
        | AuthAction::Recursive
        | AuthAction::Transaction { .. }
        | AuthAction::Savepoint { .. } => Authorization::Allow,
        AuthAction::Pragma {
            pragma_name,
            pragma_value,
        } => {
            if pragma_allowed(pragma_name, *pragma_value, privileged) {
                Authorization::Allow
            } else {
                Authorization::Deny
            }
        }
        _ => Authorization::Deny,
    }
}

fn pragma_allowed(name: &str, value: Option<&str>, privileged: bool) -> bool {
    let name = name.to_ascii_lowercase();
    if privileged && DRIVER_PRAGMAS.contains(&name.as_str()) {
        // The driver only ever switches `query_only` on.
        return name != "query_only"
            || value.is_some_and(|v| v.eq_ignore_ascii_case("on") || v == "1");
    }
    PRAGMAS_WITH_ARG.contains(&name.as_str())
        || (value.is_none() && PRAGMAS_READ.contains(&name.as_str()))
}

/// Maps a client error to a driver error. Messages come from SQLite (names, never values);
/// the statement text is never included.
pub(crate) fn map_error(e: &rusqlite::Error, read_only: bool) -> DriverError {
    match e {
        rusqlite::Error::SqliteFailure(f, msg) => {
            let message = msg.clone().unwrap_or_else(|| f.to_string());
            engine_error(*f, message, None, read_only)
        }
        // The client's own text quotes the whole statement: keep SQLite's message and turn the
        // byte offset into the 1-based character position, never the statement.
        rusqlite::Error::SqlInputError {
            error,
            msg,
            sql,
            offset,
        } => {
            let position = usize::try_from(*offset)
                .ok()
                .and_then(|o| sql.get(..o))
                .and_then(|p| u32::try_from(p.chars().count() + 1).ok());
            engine_error(*error, msg.clone(), position, read_only)
        }
        rusqlite::Error::MultipleStatement => DriverError::MultipleStatements(2),
        rusqlite::Error::InvalidParameterCount(given, expected) => DriverError::InvalidParams(
            format!("the statement has {expected} parameters, {given} given"),
        ),
        rusqlite::Error::NulError(_) => {
            DriverError::InvalidParams("the statement holds a NUL character".into())
        }
        rusqlite::Error::Utf8Error(..) => DriverError::Query {
            code: None,
            message: "the database holds text that is not valid UTF-8".into(),
            position: None,
        },
        other => DriverError::Query {
            code: None,
            message: format!("SQLite client error: {other}"),
            position: None,
        },
    }
}

fn engine_error(
    f: rusqlite::ffi::Error,
    message: String,
    position: Option<u32>,
    read_only: bool,
) -> DriverError {
    match f.code {
        ErrorCode::OperationInterrupted => DriverError::Cancelled,
        ErrorCode::ReadOnly => DriverError::ReadOnly(message),
        // A denied action is `SQLITE_AUTH`; a denied function is `SQLITE_ERROR` with the
        // same "not authorized" wording.
        ErrorCode::AuthorizationForStatementDenied | ErrorCode::Unknown
            if read_only
                && (f.code == ErrorCode::AuthorizationForStatementDenied
                    || message.starts_with("not authorized")) =>
        {
            DriverError::ReadOnly(format!(
                "statement not allowed in a read-only session ({message})"
            ))
        }
        _ => DriverError::Query {
            code: Some(f.extended_code.to_string()),
            message,
            position,
        },
    }
}

/// Days since 1970-01-01 to `(year, month, day)` (proleptic Gregorian).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = u32::try_from(doy - (153 * mp + 2) / 5 + 1).unwrap_or(1);
    let m = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap_or(1);
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `YYYY-MM-DD`, the text form SQLite's date functions read; `None` outside years 0–9999.
pub(crate) fn date_text(days: i64) -> Option<String> {
    let (y, m, d) = civil_from_days(days);
    (0..=9999)
        .contains(&y)
        .then(|| format!("{y:04}-{m:02}-{d:02}"))
}

/// `YYYY-MM-DD HH:MM:SS.ffffff`, with `Z` for UTC.
pub(crate) fn timestamp_text(micros: i64, utc: bool) -> Option<String> {
    let days = micros.div_euclid(86_400_000_000);
    let rem = micros.rem_euclid(86_400_000_000);
    let date = date_text(days)?;
    let secs = rem / 1_000_000;
    let frac = rem % 1_000_000;
    Some(format!(
        "{date} {:02}:{:02}:{:02}.{frac:06}{}",
        secs / 3600,
        secs / 60 % 60,
        secs % 60,
        if utc { "Z" } else { "" }
    ))
}

/// `true` when every parameter can be bound faithfully.
pub(crate) fn params_supported(params: &[Param]) -> bool {
    params.iter().all(|p| match p {
        Param::Null
        | Param::Bool(_)
        | Param::Int(_)
        | Param::Decimal(_)
        | Param::Text(_)
        | Param::Bytes(_)
        | Param::Json(_) => true,
        // SQLite stores NaN as NULL.
        Param::Float(f) => !f.is_nan(),
        Param::Date(d) => date_text(i64::from(*d)).is_some(),
        Param::Timestamp { micros, utc } => timestamp_text(*micros, *utc).is_some(),
        _ => false,
    })
}

fn param_value(p: &Param) -> Option<Value> {
    Some(match p {
        Param::Null => Value::Null,
        Param::Bool(b) => Value::Integer(i64::from(*b)),
        Param::Int(i) => Value::Integer(*i),
        Param::Float(f) => Value::Real(*f),
        Param::Decimal(s) | Param::Text(s) | Param::Json(s) => Value::Text(s.clone()),
        Param::Bytes(b) => Value::Blob(b.clone()),
        Param::Date(d) => Value::Text(date_text(i64::from(*d))?),
        Param::Timestamp { micros, utc } => Value::Text(timestamp_text(*micros, *utc)?),
        _ => return None,
    })
}

/// `true` for statements whose first keyword is a DML verb (an update count of 0 is meaningful).
fn is_dml(text: &str) -> bool {
    let mut rest = text.trim_start();
    loop {
        if let Some(r) = rest.strip_prefix("--") {
            rest = r.split_once('\n').map_or("", |(_, r)| r).trim_start();
        } else if let Some(r) = rest.strip_prefix("/*") {
            rest = r.split_once("*/").map_or("", |(_, r)| r).trim_start();
        } else {
            break;
        }
    }
    let word: String = rest
        .chars()
        .take_while(char::is_ascii_alphabetic)
        .collect::<String>()
        .to_ascii_uppercase();
    matches!(word.as_str(), "INSERT" | "UPDATE" | "DELETE" | "REPLACE")
}

enum Wait {
    Demand(oneshot::Sender<Chunk>),
    Gone,
    Cancelled,
    Expired,
}

enum Stop {
    /// The stream was dropped or the session is closing.
    Gone,
    Cancelled,
    /// Paused too long.
    Expired,
    Failed(DriverError),
}

/// Owns the connection; runs on the connection thread.
struct Worker {
    conn: Connection,
    shared: Arc<Shared>,
    interrupt: Arc<InterruptHandle>,
    /// Deadline of the running driver statement, in milliseconds since `epoch` (0: none).
    deadline: Arc<AtomicU64>,
    epoch: Instant,
    /// The authorizer allows the driver's own pragmas while set (driver SQL only).
    privileged: Arc<AtomicBool>,
    /// The user execution running now, read by the progress handler.
    current: Arc<Mutex<Option<Arc<ExecState>>>>,
    /// Delivered with the first result of the session (an immutable WAL open).
    startup_notice: std::cell::Cell<Option<Notice>>,
}

impl Worker {
    fn open(target: &Target, options: &OpenOptions, shared: Arc<Shared>) -> DriverResult<Self> {
        let read_only = options.read_only;
        let open_err = |e: &rusqlite::Error| match e {
            rusqlite::Error::SqliteFailure(f, _) if f.code == ErrorCode::CannotOpen => {
                DriverError::Io("cannot open the database file".into())
            }
            rusqlite::Error::SqliteFailure(f, _) if f.code == ErrorCode::PermissionDenied => {
                DriverError::Io("permission denied on the database file".into())
            }
            other => DriverError::Connect(format!(
                "cannot open the database: {}",
                map_error(other, read_only)
            )),
        };
        let mut startup_notice = None;
        let conn = match target {
            Target::File { path, create } => {
                let real = resolve_path(path, *create && !read_only)?;
                if read_only && options.immutable {
                    // A file inside a DBeaver workspace (I4): never create or modify anything
                    // next to it, whatever its journal mode and sidecar files.
                    let pending = has_sidecar(&real, "-wal") || has_sidecar(&real, "-journal");
                    tracing::warn!(pending, "DBeaver workspace file: opened immutable");
                    startup_notice = Some(Notice::new(
                        NoticeSeverity::Warning,
                        if pending {
                            "this database file is inside a DBeaver workspace: it is opened \
                             read-only and immutable so that nothing is written next to it; \
                             changes held in its -wal or -journal file are not visible, and \
                             results may be stale or fail with SQLITE_CORRUPT"
                        } else {
                            "this database file is inside a DBeaver workspace: it is opened \
                             read-only and immutable so that nothing is written next to it; if \
                             another program writes to it while this session is open, results \
                             may be stale or fail with SQLITE_CORRUPT"
                        },
                    ));
                    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
                        | OpenFlags::SQLITE_OPEN_URI
                        | OpenFlags::SQLITE_OPEN_NO_MUTEX
                        | OpenFlags::SQLITE_OPEN_NOFOLLOW;
                    Connection::open_with_flags(immutable_uri(&real), flags)
                        .map_err(|e| open_err(&e))?
                } else if read_only && wal_without_sidecars(&real) {
                    // A plain read-only open of a WAL database would create `-wal` and `-shm`
                    // next to it (I4, review M1). With neither present no writer is active and
                    // the file holds every committed page: open it immutable, through a URI the
                    // driver builds from the canonical path (user text is never a URI).
                    tracing::warn!("WAL database without -wal / -shm: opened immutable");
                    startup_notice = Some(Notice::new(
                        NoticeSeverity::Warning,
                        "this WAL-mode database is opened immutable so that no -wal or -shm file \
                         is created next to it: if another program writes to it while this \
                         session is open, results may be stale or fail with SQLITE_CORRUPT \
                         (the file itself is never modified)",
                    ));
                    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
                        | OpenFlags::SQLITE_OPEN_URI
                        | OpenFlags::SQLITE_OPEN_NO_MUTEX
                        | OpenFlags::SQLITE_OPEN_NOFOLLOW;
                    Connection::open_with_flags(immutable_uri(&real), flags)
                        .map_err(|e| open_err(&e))?
                } else {
                    // Never `SQLITE_OPEN_URI`: the path is absolute, so it is never read as a
                    // `file:` URI even with the bundled build's `SQLITE_USE_URI`.
                    let mut flags =
                        OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_NOFOLLOW;
                    if read_only {
                        flags |= OpenFlags::SQLITE_OPEN_READ_ONLY;
                    } else {
                        flags |= OpenFlags::SQLITE_OPEN_READ_WRITE;
                        if *create {
                            flags |= OpenFlags::SQLITE_OPEN_CREATE;
                        }
                    }
                    Connection::open_with_flags(&real, flags).map_err(|e| open_err(&e))?
                }
            }
            Target::Memory => {
                // A private in-memory database (never a shared cache); read-only sessions
                // refuse it before reaching here (it would be empty).
                Connection::open_in_memory_with_flags(
                    OpenFlags::SQLITE_OPEN_READ_WRITE
                        | OpenFlags::SQLITE_OPEN_CREATE
                        | OpenFlags::SQLITE_OPEN_NO_MUTEX,
                )
                .map_err(|e| open_err(&e))?
            }
        };
        let interrupt = Arc::new(conn.get_interrupt_handle());
        let mut worker = Self {
            conn,
            shared,
            interrupt,
            deadline: Arc::new(AtomicU64::new(0)),
            epoch: Instant::now(),
            privileged: Arc::new(AtomicBool::new(false)),
            current: Arc::new(Mutex::new(None)),
            startup_notice: std::cell::Cell::new(startup_notice),
        };
        worker.configure(options).map_err(|e| match e {
            DriverError::ReadOnly(m) => DriverError::ReadOnly(m),
            DriverError::Query { message, .. } => {
                DriverError::Connect(format!("cannot set up the database: {message}"))
            }
            other => other,
        })?;
        Ok(worker)
    }

    fn configure(&mut self, options: &OpenOptions) -> DriverResult<()> {
        let ro = options.read_only;
        let cfg = |e: rusqlite::Error| map_error(&e, ro);
        let c = &self.conn;
        c.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)
            .map_err(cfg)?;
        c.set_db_config(DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA, false)
            .map_err(cfg)?;
        // A double-quoted name that matches no column must not silently become a string
        // literal (an edit's `WHERE "id" = ?` would then match every row; review L2).
        c.set_db_config(DbConfig::SQLITE_DBCONFIG_DQS_DML, false)
            .map_err(cfg)?;
        c.set_db_config(DbConfig::SQLITE_DBCONFIG_DQS_DDL, false)
            .map_err(cfg)?;
        if ro {
            c.set_db_config(DbConfig::SQLITE_DBCONFIG_ENABLE_ATTACH_CREATE, false)
                .map_err(cfg)?;
            c.set_db_config(DbConfig::SQLITE_DBCONFIG_ENABLE_ATTACH_WRITE, false)
                .map_err(cfg)?;
            c.set_limit(Limit::SQLITE_LIMIT_ATTACHED, 0).map_err(cfg)?;
        }
        c.busy_timeout(options.busy_timeout).map_err(cfg)?;
        let deadline = Arc::clone(&self.deadline);
        let epoch = self.epoch;
        let shared = Arc::clone(&self.shared);
        let current = Arc::clone(&self.current);
        self.conn
            .progress_handler(
                PROGRESS_OPS,
                Some(move || {
                    // A driver statement past its deadline, a closing session, or a cancelled
                    // user statement (should an interrupt have been missed) stops here.
                    let d = deadline.load(Ordering::SeqCst);
                    (d != 0 && millis_since(epoch) >= d)
                        || shared.closing.load(Ordering::SeqCst)
                        || current
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .as_ref()
                            .is_some_and(|e| e.cancelled.load(Ordering::SeqCst))
                }),
            )
            .map_err(cfg)?;
        let privileged = Arc::clone(&self.privileged);
        self.conn
            .authorizer(Some(move |ctx: AuthContext<'_>| {
                authorize(ro, privileged.load(Ordering::SeqCst), &ctx.action)
            }))
            .map_err(cfg)?;
        // The file must be a readable database (SQLite opens lazily).
        let rows = self.query_internal("SELECT count(*) FROM sqlite_schema", &[], 1, 64)?;
        if rows.len() != 1 {
            return Err(DriverError::Connect(
                "the database schema cannot be read".into(),
            ));
        }
        self.run_privileged("PRAGMA cell_size_check = ON")?;
        for sql in &options.pragmas {
            self.run_privileged(sql)?;
        }
        if ro {
            self.run_privileged("PRAGMA query_only = ON")
                .map_err(|e| DriverError::ReadOnly(format!("cannot enable read-only mode: {e}")))?;
            self.verify_read_only()
                .map_err(|e| DriverError::ReadOnly(format!("cannot enable read-only mode: {e}")))?;
        }
        Ok(())
    }

    fn run(mut self, rx: &Receiver<Command>) {
        while let Ok(cmd) = rx.recv() {
            if let Command::Close { reply } = cmd {
                self.shutdown();
                let _ = reply.send(());
                return;
            }
            let outcome = catch_unwind(AssertUnwindSafe(|| self.handle(cmd)));
            if outcome.is_err() {
                tracing::error!("the SQLite connection thread failed; closing the session");
                self.shared
                    .mark_closed("the SQLite client failed; reconnect to continue");
            }
            if self.shared.closed.load(Ordering::SeqCst) {
                break;
            }
        }
        self.shutdown();
    }

    fn handle(&mut self, cmd: Command) {
        match cmd {
            Command::Execute(req) => self.execute(*req),
            Command::Op { op, guard, reply } => {
                let mut out = self.op(op);
                if self.shared.read_only {
                    if let Err(e) = self.after_statement() {
                        out = Err(e);
                    }
                } else {
                    self.refresh_tx();
                }
                guard.complete();
                let _ = reply.send(out);
            }
            Command::Close { .. } => {}
        }
    }

    /// Rolls back an open transaction and closes the connection.
    fn shutdown(self) {
        if !self.conn.is_autocommit() {
            let _ = self.conn.execute_batch("ROLLBACK");
        }
        self.shared.mark_closed("session closed");
        if let Err((_, e)) = self.conn.close() {
            tracing::debug!(
                driver_error_kind = crate::logkind::error_kind(&map_error(&e, false)),
                code = e.sqlite_error().map(|c| c.extended_code),
                "closing the SQLite connection failed"
            );
        }
    }

    fn refresh_tx(&self) {
        self.shared
            .in_tx
            .store(!self.conn.is_autocommit(), Ordering::SeqCst);
    }

    fn start_deadline(&self) {
        let d = millis_since(self.epoch)
            + u64::try_from(INTERNAL_TIMEOUT.as_millis()).unwrap_or(u64::MAX);
        self.deadline.store(d.max(1), Ordering::SeqCst);
    }

    fn deadline_passed(&self) -> bool {
        let d = self.deadline.load(Ordering::SeqCst);
        d != 0 && millis_since(self.epoch) >= d
    }

    /// Runs a driver statement (no rows expected), bounded by [`INTERNAL_TIMEOUT`]. A stray
    /// interrupt (a cancel racing with the end of the user statement) is retried once.
    fn run_internal(&self, sql: &str) -> DriverResult<()> {
        let attempt = || -> DriverResult<()> {
            self.start_deadline();
            let r = self
                .conn
                .prepare(sql)
                .and_then(|mut s| s.raw_execute().map(drop));
            let timed_out = self.deadline_passed();
            self.deadline.store(0, Ordering::SeqCst);
            r.map_err(|e| {
                if timed_out {
                    DriverError::Timeout(INTERNAL_TIMEOUT)
                } else {
                    map_error(&e, self.shared.read_only)
                }
            })
        };
        match attempt() {
            Err(DriverError::Cancelled) => attempt(),
            other => other,
        }
    }

    fn run_privileged(&self, sql: &str) -> DriverResult<()> {
        self.privileged.store(true, Ordering::SeqCst);
        let r = self.run_internal(sql);
        self.privileged.store(false, Ordering::SeqCst);
        r
    }

    /// Runs a driver query with bound parameters, at most `max_rows` rows of values cut to
    /// `max_value` bytes, bounded by [`INTERNAL_TIMEOUT`].
    fn query_internal(
        &self,
        sql: &str,
        params: &[&dyn ToSql],
        max_rows: usize,
        max_value: u64,
    ) -> DriverResult<Vec<Vec<Val>>> {
        let budget = FetchBudget::default()
            .with_max_value_bytes(max_value)
            .with_max_row_bytes(max_value.saturating_mul(16));
        let attempt = || -> DriverResult<Vec<Vec<Val>>> {
            self.start_deadline();
            let r = (|| -> rusqlite::Result<Vec<Vec<Val>>> {
                let mut stmt = self.conn.prepare(sql)?;
                for (i, p) in params.iter().enumerate() {
                    stmt.raw_bind_parameter(i + 1, p)?;
                }
                let n = stmt.column_count();
                let mut rows = stmt.raw_query();
                let mut out = Vec::new();
                while out.len() < max_rows {
                    let Some(row) = rows.next()? else { break };
                    let mut rb = RowBudget::new(&budget);
                    let mut vals = Vec::with_capacity(n);
                    for i in 0..n {
                        vals.push(types::copy_value(row.get_ref(i)?, &mut rb));
                    }
                    out.push(vals);
                }
                Ok(out)
            })();
            let timed_out = self.deadline_passed();
            self.deadline.store(0, Ordering::SeqCst);
            r.map_err(|e| {
                if timed_out {
                    DriverError::Timeout(INTERNAL_TIMEOUT)
                } else {
                    map_error(&e, self.shared.read_only)
                }
            })
        };
        match attempt() {
            Err(DriverError::Cancelled) => attempt(),
            other => other,
        }
    }

    /// Checks every read-only protection; any failure means the session cannot be trusted.
    fn verify_read_only(&self) -> DriverResult<()> {
        let c = &self.conn;
        let file_ro = c.is_readonly(MAIN_DB).unwrap_or(false);
        let attached = c.limit(Limit::SQLITE_LIMIT_ATTACHED).unwrap_or(-1);
        let defensive = c
            .db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE)
            .unwrap_or(false);
        let trusted = c
            .db_config(DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA)
            .unwrap_or(true);
        if !file_ro || attached != 0 || !defensive || trusted {
            return Err(DriverError::ReadOnly(
                "the read-only protections of the connection changed".into(),
            ));
        }
        let others = self.query_internal(
            "SELECT count(*) FROM pragma_database_list WHERE name NOT IN ('main', 'temp')",
            &[],
            1,
            64,
        )?;
        if others.first().and_then(|r| r.first()) != Some(&Val::Int(0)) {
            return Err(DriverError::ReadOnly("a database was attached".into()));
        }
        let q = self.query_internal("PRAGMA query_only", &[], 1, 64)?;
        if q.first().and_then(|r| r.first()) != Some(&Val::Int(1)) {
            return Err(DriverError::ReadOnly("query_only was switched off".into()));
        }
        Ok(())
    }

    /// After every statement of a read-only session: verify, re-assert `query_only`, and close
    /// the session when anything changed (fails closed). Also refreshes the transaction state.
    fn after_statement(&self) -> DriverResult<()> {
        self.refresh_tx();
        if !self.shared.read_only || self.shared.closing.load(Ordering::SeqCst) {
            // A closing session runs nothing more: no verification needed.
            return Ok(());
        }
        let outcome = self
            .verify_read_only()
            .and_then(|()| self.run_privileged("PRAGMA query_only = ON"));
        if let Err(e) = outcome {
            tracing::warn!("read-only verification failed; closing the session");
            self.shared
                .mark_closed("the read-only mode could not be verified; reconnect to continue");
            return Err(match e {
                DriverError::ReadOnly(m) => {
                    DriverError::ReadOnly(format!("{m}; the session was closed"))
                }
                other => DriverError::ReadOnly(format!(
                    "the read-only mode could not be verified ({other}); the session was closed"
                )),
            });
        }
        Ok(())
    }

    fn set_length_limit(&self, max_bytes: u64) {
        let limit = max_bytes.clamp(MIN_LENGTH_LIMIT, MAX_LENGTH_LIMIT);
        let _ = self.conn.set_limit(
            Limit::SQLITE_LIMIT_LENGTH,
            i32::try_from(limit).unwrap_or(i32::MAX),
        );
    }

    /// Waits for the next demand of the stream, watching the cancel and close flags and the
    /// paused-result bound.
    fn wait_demand(&self, demands: &DemandRx, exec: &ExecState) -> Wait {
        let since = Instant::now();
        loop {
            if exec.cancelled.load(Ordering::SeqCst) {
                return Wait::Cancelled;
            }
            if self.shared.closing.load(Ordering::SeqCst) {
                return Wait::Gone;
            }
            match demands.recv_timeout(DEMAND_POLL) {
                Ok(d) => return Wait::Demand(d),
                Err(RecvTimeoutError::Disconnected) => return Wait::Gone,
                Err(RecvTimeoutError::Timeout) => {
                    if since.elapsed() >= PAUSED_RESULT_TIMEOUT {
                        return Wait::Expired;
                    }
                }
            }
        }
    }

    fn execute(&mut self, req: ExecRequest) {
        let ExecRequest {
            text,
            params,
            budget,
            class,
            guard,
            exec,
            demands,
        } = req;
        let mut reply = match self.wait_demand(&demands, &exec) {
            Wait::Demand(r) => Some(r),
            other => {
                // Nothing ran: end the execution as asked.
                let stop = match other {
                    Wait::Cancelled => Stop::Cancelled,
                    Wait::Expired => Stop::Expired,
                    _ => Stop::Gone,
                };
                self.finish(guard, &exec, None, Err(stop));
                return;
            }
        };
        *self.current.lock().unwrap_or_else(PoisonError::into_inner) = Some(Arc::clone(&exec));
        let outcome = if exec.cancelled.load(Ordering::SeqCst) {
            Err(Stop::Cancelled)
        } else {
            self.stream_statement(&text, &params, budget, class, &exec, &demands, &mut reply)
        };
        self.finish(guard, &exec, reply, outcome);
        drop(demands);
    }

    /// Ends an execution: verification, then the guard, then the last answer (so the session
    /// is free when the consumer sees the end).
    fn finish(
        &self,
        guard: ExecutionGuard,
        exec: &ExecState,
        reply: Option<oneshot::Sender<Chunk>>,
        outcome: Result<Vec<DriverResult<ResultItem>>, Stop>,
    ) {
        // The statement is over: the progress handler must not stop the driver's own checks.
        *self.current.lock().unwrap_or_else(PoisonError::into_inner) = None;
        let check = self.after_statement();
        let mut items = match outcome {
            Ok(items) => items,
            Err(Stop::Failed(e)) => vec![Err(e)],
            Err(Stop::Cancelled) => vec![Err(DriverError::Cancelled)],
            Err(Stop::Expired) => {
                let e = DriverError::Query {
                    code: None,
                    message: format!(
                        "the result was paused for more than {} s and was closed to release \
                         its read lock; run the statement again",
                        PAUSED_RESULT_TIMEOUT.as_secs()
                    ),
                    position: None,
                };
                *exec
                    .end_error
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = Some(e.clone());
                vec![Err(e)]
            }
            Err(Stop::Gone) => {
                if self.shared.closing.load(Ordering::SeqCst) {
                    // The session was closed under a live stream: its next poll says so.
                    *exec
                        .end_error
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner) =
                        Some(DriverError::Closed("session closed".into()));
                }
                Vec::new()
            }
        };
        if let Err(e) = check {
            items.push(Err(e.clone()));
            *exec
                .end_error
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(e);
        }
        *exec.ended.lock().unwrap_or_else(PoisonError::into_inner) = true;
        guard.complete();
        if let Some(r) = reply {
            let _ = r.send(Chunk { items, done: true });
        }
    }

    #[allow(clippy::too_many_lines, clippy::too_many_arguments)]
    fn stream_statement(
        &self,
        text: &str,
        params: &[Param],
        budget: FetchBudget,
        class: StatementClass,
        exec: &ExecState,
        demands: &DemandRx,
        reply: &mut Option<oneshot::Sender<Chunk>>,
    ) -> Result<Vec<DriverResult<ResultItem>>, Stop> {
        let ro = self.shared.read_only;
        let fail = |e: &rusqlite::Error| {
            let mapped = map_error(e, ro);
            if matches!(mapped, DriverError::Cancelled) || exec.cancelled.load(Ordering::SeqCst) {
                Stop::Cancelled
            } else {
                Stop::Failed(mapped)
            }
        };
        // Manual commit: a transaction is opened before the first statement that is not
        // transaction control (JDBC semantics).
        if !self.shared.auto_commit.load(Ordering::SeqCst)
            && self.conn.is_autocommit()
            && class != StatementClass::Transaction
        {
            self.run_internal("BEGIN").map_err(Stop::Failed)?;
        }
        self.set_length_limit(budget.max_bytes);
        let total_before = self.conn.total_changes();
        let mut stmt = self.conn.prepare(text).map_err(|e| fail(&e))?;
        if stmt.parameter_count() != params.len() {
            return Err(Stop::Failed(DriverError::InvalidParams(format!(
                "the statement has {} parameters, {} given",
                stmt.parameter_count(),
                params.len()
            ))));
        }
        for (i, p) in params.iter().enumerate() {
            let v = param_value(p).ok_or_else(|| {
                Stop::Failed(DriverError::InvalidParams(
                    "a parameter cannot be represented in SQLite".into(),
                ))
            })?;
            stmt.raw_bind_parameter(i + 1, v).map_err(|e| fail(&e))?;
        }
        let ncols = stmt.column_count();
        // Names come from `Statement::column_name`, which reports a bad index as an error; but
        // rusqlite 0.42 still `expect`s valid UTF-8 in it and in declared types (column.rs), and
        // a hostile file can hold any bytes: the panic is caught and becomes an error (review
        // L4). This needs unwinding: see the `panic = "abort"` note in this crate's Cargo.toml.
        let columns = catch_unwind(AssertUnwindSafe(|| -> rusqlite::Result<Vec<_>> {
            let mut out = Vec::with_capacity(ncols);
            for i in 0..ncols {
                let (name, _) = truncate_str(stmt.column_name(i)?, MAX_COLUMN_NAME as u64);
                out.push(name.to_owned());
            }
            let decls: Vec<Option<String>> = stmt
                .columns()
                .map(|c| c.decl_type().map(str::to_owned))
                .collect();
            Ok(out.into_iter().zip(decls).collect::<Vec<_>>())
        }))
        .map_err(|_| {
            Stop::Failed(DriverError::Query {
                code: None,
                message: "a column name or declared type is not valid UTF-8".into(),
                position: None,
            })
        })?
        .map_err(|e| fail(&e))?;
        let (names, decls): (Vec<String>, Vec<Option<String>>) = columns.into_iter().unzip();
        let writes = !stmt.readonly();
        let mut rows = stmt.raw_query();
        let mut kinds: Option<Vec<types::Kind>> = None;
        let mut header = None;
        let mut warned = vec![false; ncols];
        let mut items: Vec<DriverResult<ResultItem>> = Vec::new();
        if let Some(n) = self.startup_notice.take() {
            items.push(Ok(ResultItem::Notice(n)));
        }
        let max_value = budget.effective_max_value_bytes();
        loop {
            let mut batch: Vec<Vec<Val>> = Vec::new();
            let mut limiter = BatchLimiter::new(&budget);
            let mut done = false;
            loop {
                if exec.cancelled.load(Ordering::SeqCst) {
                    return Err(Stop::Cancelled);
                }
                if self.shared.closing.load(Ordering::SeqCst) {
                    return Err(Stop::Gone);
                }
                match rows.next() {
                    Ok(Some(row)) => {
                        let mut rb = RowBudget::new(&budget);
                        let mut vals = Vec::with_capacity(ncols);
                        for i in 0..ncols {
                            vals.push(types::copy_value(
                                row.get_ref(i).map_err(|e| fail(&e))?,
                                &mut rb,
                            ));
                        }
                        limiter.add_row(rb.used());
                        batch.push(vals);
                        if limiter.is_full() {
                            break;
                        }
                    }
                    Ok(None) => {
                        done = true;
                        break;
                    }
                    Err(e) => return Err(fail(&e)),
                }
            }
            if ncols > 0 {
                if kinds.is_none() {
                    let k = types::decide_kinds(&decls, &batch);
                    let h = types::schema(&names, &k);
                    items.push(Ok(ResultItem::ResultSet(ResultSetHeader::tabular(
                        0,
                        Arc::clone(&h),
                    ))));
                    kinds = Some(k);
                    header = Some(h);
                }
                if let (Some(k), Some(h)) = (&kinds, &header)
                    && !batch.is_empty()
                {
                    let (rb, mixed) =
                        types::build_batch(h, k, &batch, max_value).map_err(Stop::Failed)?;
                    drop(batch);
                    items.push(Ok(ResultItem::Rows(rb)));
                    for (i, m) in mixed.iter().enumerate() {
                        if *m && !warned[i] {
                            warned[i] = true;
                            items.push(Ok(ResultItem::Notice(Notice::new(
                                NoticeSeverity::Warning,
                                format!(
                                    "column `{}` holds values of another storage class than \
                                     its type; they are shown converted or as NULL, and the \
                                     column is not editable",
                                    names[i]
                                ),
                            ))));
                        }
                    }
                }
            }
            if done {
                break;
            }
            // Deliver this batch and wait until the consumer asks for the next one.
            if let Some(r) = reply.take() {
                let _ = r.send(Chunk {
                    items: std::mem::take(&mut items),
                    done: false,
                });
            }
            match self.wait_demand(demands, exec) {
                Wait::Demand(r) => *reply = Some(r),
                Wait::Gone => return Err(Stop::Gone),
                Wait::Cancelled => return Err(Stop::Cancelled),
                Wait::Expired => return Err(Stop::Expired),
            }
        }
        drop(rows);
        drop(stmt);
        if ncols == 0 || writes {
            let delta = self.conn.total_changes().saturating_sub(total_before);
            let count = if delta > 0 {
                Some(self.conn.changes())
            } else if is_dml(text) {
                Some(0)
            } else {
                None
            };
            if ncols == 0 || count.is_some() {
                items.push(Ok(ResultItem::UpdateCount(count)));
            }
        }
        Ok(items)
    }

    fn op(&self, op: Op) -> DriverResult<OpOut> {
        match op {
            Op::Sql(sql) => self.run_internal(&sql).map(|()| OpOut::Done),
            Op::Commit => {
                if !self.conn.is_autocommit() {
                    self.run_internal("COMMIT")?;
                }
                Ok(OpOut::Done)
            }
            Op::Rollback => {
                if !self.conn.is_autocommit() {
                    self.run_internal("ROLLBACK")?;
                }
                Ok(OpOut::Done)
            }
            Op::SetAutoCommit(on) => {
                // Back to auto-commit commits the open transaction (JDBC semantics).
                if on && !self.conn.is_autocommit() {
                    self.run_internal("COMMIT")?;
                }
                self.shared.auto_commit.store(on, Ordering::SeqCst);
                Ok(OpOut::Done)
            }
            Op::Ping => self
                .query_internal("SELECT 1", &[], 1, 64)
                .map(|_| OpOut::Done),
            Op::List { parent, kind, page } => self.list(&parent, kind, &page).map(OpOut::Page),
            Op::Ddl(path) => self.ddl(&path).map(OpOut::Ddl),
        }
    }

    /// Quoted name of an existing schema (`main`, `temp`, an attached database).
    fn schema_ident(&self, name: &str) -> DriverResult<String> {
        let rows = self.query_internal(
            "SELECT 1 FROM pragma_database_list WHERE name = ?1",
            &[&name],
            1,
            64,
        )?;
        if rows.is_empty() {
            return Err(DriverError::InvalidParams("unknown schema".into()));
        }
        Ok(SqliteDialect.quote_identifier(name))
    }

    fn list(
        &self,
        parent: &ObjectPath,
        kind: ObjectKind,
        page: &PageRequest,
    ) -> DriverResult<Page<ObjectInfo>> {
        const NAME_BYTES: u64 = 64 * 1024;
        let limit = page.limit.clamp(1, 10_000) as usize;
        let fetch = i64::try_from(limit + 1).unwrap_or(i64::MAX);
        let filter = page.filter.as_deref().map(|f| escape_like(f, '!'));
        let cursor = page.cursor.clone();
        let segment = |idx: usize, kinds: &[ObjectKind]| -> DriverResult<String> {
            parent
                .0
                .get(idx)
                .filter(|s| kinds.contains(&s.kind))
                .map(|s| s.name.clone())
                .ok_or_else(|| DriverError::InvalidParams("unexpected object path".into()))
        };
        let mut items = Vec::new();
        let mut next = None;
        let more;
        match (parent.kind(), kind) {
            (None, ObjectKind::Schema) => {
                let rows = self.query_internal(
                    "SELECT name FROM pragma_database_list \
                     WHERE (?1 IS NULL OR name LIKE ?1 ESCAPE '!') AND (?2 IS NULL OR name > ?2) \
                     ORDER BY name LIMIT ?3",
                    &[&filter, &cursor, &fetch],
                    limit + 1,
                    NAME_BYTES,
                )?;
                more = rows.len() > limit;
                for row in rows.iter().take(limit) {
                    let name = text(row.first());
                    next = Some(name.clone());
                    items.push(ObjectInfo {
                        path: parent.child(kind, name),
                        comment: None,
                        details: ObjectDetails::None,
                    });
                }
            }
            (Some(ObjectKind::Schema), ObjectKind::Table | ObjectKind::View) => {
                let schema = self.schema_ident(&segment(0, &[ObjectKind::Schema])?)?;
                let ty = if kind == ObjectKind::Table {
                    "table"
                } else {
                    "view"
                };
                let sql = format!(
                    "SELECT name, sql LIKE 'CREATE VIRTUAL TABLE%' FROM {schema}.sqlite_schema \
                     WHERE type = ?1 AND name NOT LIKE 'sqlite!_%' ESCAPE '!' \
                     AND (?2 IS NULL OR name LIKE ?2 ESCAPE '!') AND (?3 IS NULL OR name > ?3) \
                     ORDER BY name LIMIT ?4"
                );
                let rows = self.query_internal(
                    &sql,
                    &[&ty, &filter, &cursor, &fetch],
                    limit + 1,
                    NAME_BYTES,
                )?;
                more = rows.len() > limit;
                for row in rows.iter().take(limit) {
                    let name = text(row.first());
                    next = Some(name.clone());
                    let mut t = TableInfo::default();
                    if row.get(1) == Some(&Val::Int(1)) {
                        t.flavour = Some("virtual".to_owned());
                    } else {
                        // Ordinary SQLite tables are always transactional; a virtual table's
                        // module decides (unknown).
                        t.transactional = Some(true);
                    }
                    items.push(ObjectInfo {
                        path: parent.child(kind, name),
                        comment: None,
                        details: ObjectDetails::Table(t),
                    });
                }
            }
            (Some(ObjectKind::Table | ObjectKind::View), ObjectKind::Column) => {
                let schema = segment(0, &[ObjectKind::Schema])?;
                let table = segment(1, &[ObjectKind::Table, ObjectKind::View])?;
                let after: Option<i64> =
                    match &cursor {
                        Some(c) => Some(c.parse().map_err(|_| {
                            DriverError::InvalidParams("invalid page cursor".into())
                        })?),
                        None => None,
                    };
                let rows = self.query_internal(
                    "SELECT cid, name, type, \"notnull\", dflt_value, pk, hidden \
                     FROM pragma_table_xinfo(?1, ?2) \
                     WHERE (?3 IS NULL OR name LIKE ?3 ESCAPE '!') AND (?4 IS NULL OR cid > ?4) \
                     ORDER BY cid LIMIT ?5",
                    &[&table, &schema, &filter, &after, &fetch],
                    limit + 1,
                    NAME_BYTES,
                )?;
                more = rows.len() > limit;
                for row in rows.iter().take(limit) {
                    let cid = match row.first() {
                        Some(Val::Int(i)) => *i,
                        _ => 0,
                    };
                    let ordinal = u32::try_from(cid + 1).unwrap_or(0);
                    let mut c = ColumnInfo::new(ordinal, text(row.get(2)));
                    c.nullable = row.get(3) != Some(&Val::Int(1));
                    c.default = match row.get(4) {
                        Some(Val::Null) | None => None,
                        v => Some(text(v)),
                    };
                    c.primary_key = matches!(row.get(5), Some(Val::Int(n)) if *n > 0);
                    c.auto_generated = matches!(row.get(6), Some(Val::Int(2 | 3)));
                    next = Some(cid.to_string());
                    items.push(ObjectInfo {
                        path: parent.child(kind, text(row.get(1))),
                        comment: None,
                        details: ObjectDetails::Column(c),
                    });
                }
            }
            (
                Some(ObjectKind::Table | ObjectKind::View),
                ObjectKind::Index | ObjectKind::Trigger,
            ) if kind == ObjectKind::Trigger || parent.kind() == Some(ObjectKind::Table) => {
                let schema_name = segment(0, &[ObjectKind::Schema])?;
                let table = segment(1, &[ObjectKind::Table, ObjectKind::View])?;
                if kind == ObjectKind::Index {
                    let rows = self.query_internal(
                        "SELECT name, \"unique\", origin, partial FROM pragma_index_list(?1, ?2) \
                         WHERE (?3 IS NULL OR name LIKE ?3 ESCAPE '!') AND (?4 IS NULL OR name > ?4) \
                         ORDER BY name LIMIT ?5",
                        &[&table, &schema_name, &filter, &cursor, &fetch],
                        limit + 1,
                        NAME_BYTES,
                    )?;
                    more = rows.len() > limit;
                    for row in rows.iter().take(limit) {
                        let name = text(row.first());
                        let mut info = IndexInfo::default();
                        info.unique = row.get(1) == Some(&Val::Int(1));
                        info.primary = text(row.get(2)) == "pk";
                        // SQLite indexes are always valid and checked at once; `NULL`s are
                        // distinct. A partial index is flagged; its predicate text is not read
                        // (`predicate` stays `None`).
                        info.valid = true;
                        info.deferrable = false;
                        info.nulls_distinct = true;
                        info.partial = row.get(3) == Some(&Val::Int(1));
                        // Key parts; SQLite does not report the text of an expression part.
                        let parts = self.query_internal(
                            "SELECT name, cid FROM pragma_index_xinfo(?1, ?2) \
                             WHERE key = 1 ORDER BY seqno LIMIT 2001",
                            &[&name, &schema_name],
                            2001,
                            NAME_BYTES,
                        )?;
                        info.columns = parts
                            .iter()
                            .map(|p| match p.first() {
                                Some(Val::Null) | None => "(expression)".to_owned(),
                                v => text(v),
                            })
                            .collect();
                        // `cid` -2 is an expression part (and its name is NULL).
                        info.expressions = parts
                            .iter()
                            .map(|p| {
                                matches!(p.first(), Some(Val::Null) | None)
                                    || p.get(1) == Some(&Val::Int(-2))
                            })
                            .collect();
                        next = Some(name.clone());
                        items.push(ObjectInfo {
                            path: parent.child(kind, name),
                            comment: None,
                            details: ObjectDetails::Index(info),
                        });
                    }
                } else {
                    let schema = self.schema_ident(&schema_name)?;
                    let sql = format!(
                        "SELECT name, substr(sql, 1, 2048) FROM {schema}.sqlite_schema \
                         WHERE type = 'trigger' AND tbl_name = ?1 \
                         AND (?2 IS NULL OR name LIKE ?2 ESCAPE '!') AND (?3 IS NULL OR name > ?3) \
                         ORDER BY name LIMIT ?4"
                    );
                    let rows = self.query_internal(
                        &sql,
                        &[&table, &filter, &cursor, &fetch],
                        limit + 1,
                        // The trigger's header is at the start of its text.
                        NAME_BYTES,
                    )?;
                    more = rows.len() > limit;
                    for row in rows.iter().take(limit) {
                        let name = text(row.first());
                        next = Some(name.clone());
                        items.push(ObjectInfo {
                            path: parent.child(kind, name),
                            comment: None,
                            details: ObjectDetails::Trigger(trigger_info(&text(row.get(1)))),
                        });
                    }
                }
            }
            (Some(ObjectKind::Table), ObjectKind::ForeignKey) => {
                let schema_name = segment(0, &[ObjectKind::Schema])?;
                let table = segment(1, &[ObjectKind::Table])?;
                // Foreign keys have no name in SQLite: `fk_<id>`, keyed by the engine's id.
                let after: Option<i64> = match &cursor {
                    Some(c) => Some(
                        c.strip_prefix("fk_")
                            .and_then(|n| n.parse().ok())
                            .ok_or_else(|| {
                                DriverError::InvalidParams("invalid page cursor".into())
                            })?,
                    ),
                    None => None,
                };
                let rows = self.query_internal(
                    "SELECT id, \"table\", \"from\", \"to\", on_update, on_delete \
                     FROM pragma_foreign_key_list(?1, ?2) WHERE (?3 IS NULL OR id > ?3) \
                     ORDER BY id, seq LIMIT 10001",
                    &[&table, &schema_name, &after],
                    10_001,
                    NAME_BYTES,
                )?;
                let pattern = page.filter.as_deref().map(str::to_lowercase);
                let mut keys: Vec<(String, ForeignKeyInfo, bool)> = Vec::new();
                for row in &rows {
                    let id = match row.first() {
                        Some(Val::Int(i)) => *i,
                        _ => 0,
                    };
                    let name = format!("fk_{id}");
                    if keys.last().is_none_or(|(n, _, _)| *n != name) {
                        let target = ObjectPath::root()
                            .child(ObjectKind::Schema, schema_name.clone())
                            .child(ObjectKind::Table, text(row.get(1)));
                        let mut f = ForeignKeyInfo::new(target);
                        f.on_update = ReferentialAction::parse(&text(row.get(4)));
                        f.on_delete = ReferentialAction::parse(&text(row.get(5)));
                        keys.push((name, f, false));
                    }
                    if let Some((_, f, implicit)) = keys.last_mut() {
                        f.columns.push(text(row.get(2)));
                        match row.get(3) {
                            Some(Val::Null) | None => *implicit = true,
                            v => f.referenced_columns.push(text(v)),
                        }
                    }
                }
                // At the row cap the last key may be partial: drop it and continue from the
                // last complete one on the next page (security review L4).
                let capped = rows.len() >= 10_001;
                if capped {
                    keys.pop();
                }
                let last_complete = keys.last().map(|(n, _, _)| n.clone());
                // The filter matches the name or the referenced table, as a literal substring.
                keys.retain(|(n, f, _)| {
                    pattern.as_ref().is_none_or(|p| {
                        n.to_lowercase().contains(p)
                            || f.referenced_table
                                .leaf()
                                .is_some_and(|t| t.name.to_lowercase().contains(p))
                    })
                });
                more = keys.len() > limit || (capped && last_complete.is_some());
                // Every key left fits the page: continue after the last one read.
                let tail = if keys.len() <= limit && capped {
                    last_complete
                } else {
                    None
                };
                for (name, mut f, implicit) in keys.into_iter().take(limit) {
                    if implicit {
                        // `REFERENCES t` without columns: the referenced table's primary key.
                        let target = f
                            .referenced_table
                            .leaf()
                            .map(|t| t.name.clone())
                            .unwrap_or_default();
                        let pk = self.query_internal(
                            "SELECT name FROM pragma_table_info(?1, ?2) WHERE pk > 0 \
                             ORDER BY pk LIMIT 2001",
                            &[&target, &schema_name],
                            2001,
                            NAME_BYTES,
                        )?;
                        f.referenced_columns = pk.iter().map(|r| text(r.first())).collect();
                    }
                    next = Some(name.clone());
                    items.push(ObjectInfo {
                        path: parent.child(kind, name),
                        comment: None,
                        details: ObjectDetails::ForeignKey(f),
                    });
                }
                if tail.is_some() {
                    next = tail;
                }
            }
            _ => {
                return Err(DriverError::Unsupported(
                    ninesql_driver_api::Feature::Metadata(kind),
                ));
            }
        }
        Ok(Page {
            items,
            next_cursor: if more { next } else { None },
        })
    }

    fn ddl(&self, path: &ObjectPath) -> DriverResult<String> {
        let leaf = path
            .leaf()
            .ok_or_else(|| DriverError::InvalidParams("empty object path".into()))?;
        let (ty, depth) = match leaf.kind {
            ObjectKind::Table => ("table", 2),
            ObjectKind::View => ("view", 2),
            ObjectKind::Index => ("index", 3),
            ObjectKind::Trigger => ("trigger", 3),
            _ => return Err(DriverError::Unsupported(ninesql_driver_api::Feature::Ddl)),
        };
        let schema = path
            .0
            .first()
            .filter(|s| s.kind == ObjectKind::Schema && path.0.len() == depth)
            .ok_or_else(|| DriverError::InvalidParams("unexpected object path".into()))?;
        // An index or trigger belongs to the table (or view) of its path.
        let owner = if depth == 3 {
            match path.0.get(1) {
                Some(t)
                    if t.kind == ObjectKind::Table
                        || (t.kind == ObjectKind::View && leaf.kind == ObjectKind::Trigger) =>
                {
                    Some(t.name.clone())
                }
                _ => return Err(DriverError::InvalidParams("unexpected object path".into())),
            }
        } else {
            None
        };
        let schema = self.schema_ident(&schema.name)?;
        let sql = format!(
            "SELECT sql, length(CAST(sql AS BLOB)) FROM {schema}.sqlite_schema \
             WHERE type = ?1 AND name = ?2 AND (?3 IS NULL OR tbl_name = ?3)"
        );
        let rows = self.query_internal(&sql, &[&ty, &leaf.name, &owner], 1, MAX_DDL_BYTES)?;
        let row = rows
            .first()
            .ok_or_else(|| DriverError::InvalidParams("object not found".into()))?;
        if row.first() == Some(&Val::Null) {
            return Err(DriverError::Query {
                code: None,
                message: "this index was created by a constraint: its DDL is part of the \
                          table's"
                    .into(),
                position: None,
            });
        }
        if matches!(row.get(1), Some(Val::Int(n)) if *n > i64::try_from(MAX_DDL_BYTES).unwrap_or(i64::MAX))
        {
            return Err(DriverError::Query {
                code: None,
                message: "the DDL of this object is larger than 1 MiB".into(),
                position: None,
            });
        }
        Ok(text(row.first()))
    }
}

/// Timing and event of a trigger, read from its stored `CREATE TRIGGER` text (SQLite has no
/// catalog columns for them). Quoted names and comments are skipped as whole tokens.
fn trigger_info(sql: &str) -> TriggerInfo {
    let mut words: Vec<String> = Vec::new();
    let mut it = sql.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            c if c.is_whitespace() => {}
            '-' if it.peek() == Some(&'-') => {
                for n in it.by_ref() {
                    if n == '\n' {
                        break;
                    }
                }
            }
            '/' if it.peek() == Some(&'*') => {
                it.next();
                let mut prev = ' ';
                for n in it.by_ref() {
                    if prev == '*' && n == '/' {
                        break;
                    }
                    prev = n;
                }
            }
            '\'' | '"' | '`' | '[' => {
                let close = if c == '[' { ']' } else { c };
                while let Some(n) = it.next() {
                    if n == close {
                        // A doubled quote stays inside the token.
                        if close != ']' && it.peek() == Some(&close) {
                            it.next();
                            continue;
                        }
                        break;
                    }
                }
                words.push("\u{0}".to_owned());
            }
            c if c.is_alphanumeric() || c == '_' || c == '$' || !c.is_ascii() => {
                let mut w = String::from(c);
                while let Some(&n) = it.peek() {
                    if n.is_alphanumeric() || n == '_' || n == '$' || !n.is_ascii() {
                        w.push(n);
                        it.next();
                    } else {
                        break;
                    }
                }
                words.push(w.to_ascii_uppercase());
            }
            c => words.push(c.to_string()),
        }
        if words.len() > 64 {
            break;
        }
    }
    let mut t = TriggerInfo::default();
    t.for_each_row = true;
    t.enabled = true;
    let Some(mut i) = words.iter().position(|w| w == "TRIGGER") else {
        return t;
    };
    i += 1;
    if words.get(i).map(String::as_str) == Some("IF") {
        i += 3;
    }
    // The trigger's name, possibly schema-qualified.
    i += 1;
    if words.get(i).map(String::as_str) == Some(".") {
        i += 2;
    }
    let mut timing = "BEFORE";
    while let Some(w) = words.get(i) {
        match w.as_str() {
            "BEFORE" => timing = "BEFORE",
            "AFTER" => timing = "AFTER",
            "INSTEAD" => timing = "INSTEAD OF",
            "DELETE" | "INSERT" | "UPDATE" => t.events.push(w.clone()),
            "OF" => {}
            _ => break,
        }
        i += 1;
    }
    t.timing = Some(timing.to_owned());
    t
}

fn millis_since(epoch: Instant) -> u64 {
    u64::try_from(epoch.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Text of an internal value (lossy for invalid UTF-8, numbers in their text form).
fn text(v: Option<&Val>) -> String {
    match v {
        Some(Val::Text(b, _) | Val::Blob(b, _)) => String::from_utf8_lossy(b).into_owned(),
        Some(Val::Int(i)) => i.to_string(),
        Some(Val::Real(f)) => format!("{f:?}"),
        Some(Val::Null) | None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trigger_text_gives_timing_and_events() {
        let t = trigger_info("CREATE TRIGGER t AFTER UPDATE OF a ON x BEGIN SELECT 1; END");
        assert_eq!(t.timing.as_deref(), Some("AFTER"));
        assert_eq!(t.events, vec!["UPDATE"]);
        assert!(t.for_each_row && t.enabled);
        let t = trigger_info(
            "CREATE TEMP TRIGGER IF NOT EXISTS main.\"before \"\"x\"\"\" -- c\n INSTEAD OF INSERT ON v BEGIN SELECT 1; END",
        );
        assert_eq!(t.timing.as_deref(), Some("INSTEAD OF"));
        assert_eq!(t.events, vec!["INSERT"]);
        // A trigger named like a keyword; the default timing is BEFORE.
        let t = trigger_info("create trigger after delete on x begin select 1; end");
        assert_eq!(t.timing.as_deref(), Some("BEFORE"));
        assert_eq!(t.events, vec!["DELETE"]);
        let t = trigger_info("CREATE TRIGGER [a b] /* x */ AFTER INSERT ON x BEGIN SELECT 1; END");
        assert_eq!(t.timing.as_deref(), Some("AFTER"));
        assert_eq!(t.events, vec!["INSERT"]);
        assert_eq!(trigger_info("").events, Vec::<String>::new());
    }

    #[test]
    fn authorizer_allows_only_reads_in_read_only_sessions() {
        use AuthAction as A;
        let deny = |ro, a: A<'_>| authorize(ro, false, &a) == Authorization::Deny;
        for a in [
            A::Insert { table_name: "t" },
            A::Update {
                table_name: "t",
                column_name: "a",
            },
            A::Delete { table_name: "t" },
            A::CreateTempTable { table_name: "x" },
            A::CreateTable { table_name: "x" },
            A::Attach {
                filename: "/tmp/x.db",
            },
            A::Detach { database_name: "x" },
            A::Analyze { table_name: "t" },
            A::Reindex { index_name: "i" },
            A::Pragma {
                pragma_name: "query_only",
                pragma_value: Some("OFF"),
            },
            A::Pragma {
                pragma_name: "writable_schema",
                pragma_value: Some("ON"),
            },
            A::Pragma {
                pragma_name: "temp_store",
                pragma_value: Some("MEMORY"),
            },
            A::Pragma {
                pragma_name: "journal_mode",
                pragma_value: Some("WAL"),
            },
            A::Pragma {
                pragma_name: "optimize",
                pragma_value: None,
            },
            A::Function {
                function_name: "load_extension",
            },
            A::Function {
                function_name: "LOAD_EXTENSION",
            },
            A::Unknown {
                code: 99,
                arg1: None,
                arg2: None,
            },
        ] {
            assert!(deny(true, a), "{a:?}");
        }
        for a in [
            A::Select,
            A::Read {
                table_name: "t",
                column_name: "a",
            },
            A::Recursive,
            A::Pragma {
                pragma_name: "table_xinfo",
                pragma_value: Some("t"),
            },
            A::Pragma {
                pragma_name: "query_only",
                pragma_value: None,
            },
            A::Function {
                function_name: "count",
            },
        ] {
            assert!(!deny(true, a), "{a:?}");
        }
        // Read-write sessions: everything but the denied functions.
        assert!(!deny(false, A::Insert { table_name: "t" }));
        assert!(!deny(false, A::Attach { filename: "x" }));
        assert!(deny(
            false,
            A::Function {
                function_name: "load_extension"
            }
        ));
        // The driver's own pragmas, only while privileged, and `query_only` only switched on.
        let q = |v| A::Pragma {
            pragma_name: "query_only",
            pragma_value: Some(v),
        };
        assert_eq!(authorize(true, true, &q("ON")), Authorization::Allow);
        assert_eq!(authorize(true, true, &q("OFF")), Authorization::Deny);
        assert_eq!(authorize(true, true, &q("0")), Authorization::Deny);
        assert_eq!(
            authorize(
                true,
                true,
                &A::Pragma {
                    pragma_name: "writable_schema",
                    pragma_value: Some("ON")
                }
            ),
            Authorization::Deny
        );
    }

    fn make_db(dir: &Path) -> PathBuf {
        let path = dir.join("ro.db");
        let c = Connection::open(&path).unwrap();
        c.execute_batch(
            "CREATE TABLE t (a INTEGER PRIMARY KEY, b TEXT); CREATE INDEX t_b ON t (b); \
             INSERT INTO t VALUES (1, 'x'); \
             CREATE TABLE log (n INTEGER); \
             CREATE TRIGGER t_ai AFTER INSERT ON t BEGIN INSERT INTO log VALUES (1); END; \
             CREATE VIEW v AS SELECT a FROM t;",
        )
        .unwrap();
        drop(c);
        path
    }

    fn open(path: &Path, read_only: bool) -> Worker {
        let options = OpenOptions {
            read_only,
            immutable: false,
            busy_timeout: Duration::from_millis(200),
            pragmas: Vec::new(),
        };
        let target = Target::File {
            path: path.to_owned(),
            create: false,
        };
        match Worker::open(&target, &options, Arc::new(Shared::new(read_only, true))) {
            Ok(w) => w,
            Err(e) => panic!("open failed: {e}"),
        }
    }

    /// Runs `sql` on the connection directly: no classifier, only the engine-side layers.
    fn raw(w: &Worker, sql: &str) -> Result<(), DriverError> {
        let run = || -> rusqlite::Result<()> {
            let mut s = w.conn.prepare(sql)?;
            let mut rows = s.raw_query();
            while rows.next()?.is_some() {}
            Ok(())
        };
        run().map_err(|e| map_error(&e, w.shared.read_only))
    }

    fn snapshot(dir: &Path) -> Vec<(String, Vec<u8>)> {
        let mut out: Vec<(String, Vec<u8>)> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| {
                let e = e.unwrap();
                let bytes = std::fs::read(e.path()).unwrap_or_default();
                (e.file_name().to_string_lossy().into_owned(), bytes)
            })
            .collect();
        out.sort();
        out
    }

    /// Every bypass of the task list, sent past the client-side classifier: the engine-side
    /// layers (open mode, `query_only`, attach limits, authorizer) must stop each one, and no
    /// file in the directory may change or appear (I3, I4).
    #[test]
    fn read_only_bypasses_fail_engine_side() {
        let dir = tempfile::tempdir().unwrap();
        let path = make_db(dir.path());
        let before = snapshot(dir.path());
        let w = open(&path, true);
        let target = dir.path().join("copy.db");
        let other = dir.path().join("other.db");
        let bypasses = [
            "INSERT INTO t VALUES (2, 'y')".to_owned(),
            "UPDATE t SET b = 'z'".to_owned(),
            "DELETE FROM t".to_owned(),
            "CREATE TABLE n (a)".to_owned(),
            "DROP VIEW v".to_owned(),
            // Temporary objects are writable on a read-only file without `query_only`.
            "CREATE TEMP TABLE tmp (a)".to_owned(),
            "CREATE TEMP VIEW tv AS SELECT 1".to_owned(),
            "CREATE TEMP TRIGGER tt AFTER INSERT ON t BEGIN SELECT 1; END".to_owned(),
            // `VACUUM INTO` writes a new file even from a read-only connection.
            format!("VACUUM INTO '{}'", target.display()),
            "VACUUM".to_owned(),
            format!("ATTACH DATABASE '{}' AS other", other.display()),
            format!(
                "ATTACH DATABASE 'file:{}?mode=rwc' AS other",
                other.display()
            ),
            "ATTACH DATABASE ':memory:' AS mem".to_owned(),
            "SELECT load_extension('/nonexistent/evil')".to_owned(),
            "SELECT load_extension('/nonexistent/evil', 'entry')".to_owned(),
            "PRAGMA query_only = OFF".to_owned(),
            "PRAGMA query_only(0)".to_owned(),
            "PRAGMA main.query_only = false".to_owned(),
            "PRAGMA writable_schema = ON".to_owned(),
            "PRAGMA temp_store = FILE".to_owned(),
            "PRAGMA journal_mode = WAL".to_owned(),
            "PRAGMA user_version = 7".to_owned(),
            "PRAGMA optimize".to_owned(),
            "ANALYZE".to_owned(),
            "REINDEX".to_owned(),
            "REPLACE INTO t VALUES (1, 'w')".to_owned(),
            "INSERT INTO t SELECT a + 10, b FROM t RETURNING a".to_owned(),
            // A write lock is refused too (`query_only`).
            "BEGIN IMMEDIATE".to_owned(),
            "BEGIN EXCLUSIVE".to_owned(),
        ];
        for sql in &bypasses {
            let r = raw(&w, sql);
            assert!(matches!(r, Err(DriverError::ReadOnly(_))), "{sql} -> {r:?}");
            // Nothing lifted: the post-statement verification still passes.
            w.after_statement().unwrap();
        }
        // Reads, schema pragmas and read transactions still work.
        for sql in [
            "SELECT * FROM t",
            "SELECT * FROM v",
            "PRAGMA table_info(t)",
            "SELECT * FROM pragma_table_xinfo('t')",
            "PRAGMA query_only",
            "BEGIN",
            "SELECT count(*) FROM t",
            "COMMIT",
            "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 5) \
             SELECT * FROM c",
        ] {
            raw(&w, sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
        }
        w.after_statement().unwrap();
        w.shutdown();
        assert_eq!(snapshot(dir.path()), before, "a file changed or appeared");
    }

    /// A lifted `query_only` (impossible from SQL: here the authorizer is removed by hand) is
    /// detected after the statement and closes the session.
    #[test]
    fn a_lifted_query_only_closes_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let path = make_db(dir.path());
        let mut w = open(&path, true);
        w.conn
            .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
            .unwrap();
        raw(&w, "PRAGMA query_only = OFF").unwrap();
        let r = w.after_statement();
        assert!(matches!(r, Err(DriverError::ReadOnly(_))), "{r:?}");
        assert!(w.shared.closed.load(Ordering::SeqCst));
        assert!(matches!(w.shared.closed_error(), DriverError::Closed(_)));
    }

    /// Read-write sessions on a hostile file: views and triggers cannot call `load_extension`
    /// (denied, and `trusted_schema` off), `writable_schema` has no effect (defensive mode).
    #[test]
    fn hostile_schema_is_contained_in_read_write_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hostile.db");
        {
            let c = Connection::open(&path).unwrap();
            c.execute_batch(
                "CREATE TABLE t (a); \
                 CREATE VIEW evil AS SELECT load_extension('/nonexistent/evil') AS x; \
                 CREATE TRIGGER evil_ai AFTER INSERT ON t \
                 BEGIN SELECT load_extension('/nonexistent/evil'); END;",
            )
            .unwrap();
        }
        let w = open(&path, false);
        assert!(raw(&w, "SELECT * FROM evil").is_err());
        assert!(raw(&w, "INSERT INTO t VALUES (1)").is_err());
        assert!(raw(&w, "SELECT load_extension('/nonexistent/evil')").is_err());
        // The protections cannot be lifted, read-write or not (review L1).
        for sql in [
            "PRAGMA writable_schema = ON",
            "PRAGMA trusted_schema = 1",
            "PRAGMA main.trusted_schema(1)",
            "PRAGMA cell_size_check = 0",
        ] {
            assert!(raw(&w, sql).is_err(), "{sql}");
        }
        raw(&w, "PRAGMA trusted_schema").unwrap();
        assert!(raw(&w, "UPDATE sqlite_schema SET sql = 'x' WHERE name = 't'").is_err());
        // Double-quoted strings are identifiers only (review L2).
        assert!(raw(&w, "SELECT * FROM t WHERE a = \"no_such_column\"").is_err());
        // Ordinary writes work.
        raw(&w, "CREATE TABLE ok (a)").unwrap();
        raw(&w, "INSERT INTO ok VALUES (1)").unwrap();
    }

    /// With `DQS_DDL` off, a schema written by an older tool with double-quoted string literals
    /// still loads and works (SQLite parses stored schemas leniently).
    #[test]
    fn legacy_double_quoted_schema_still_opens() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.db");
        {
            let c = Connection::open(&path).unwrap();
            c.set_db_config(DbConfig::SQLITE_DBCONFIG_DQS_DDL, true)
                .unwrap();
            c.execute_batch(
                "CREATE TABLE t (a TEXT DEFAULT \"x\", b INTEGER CHECK (a <> \"bad\")); \
                 INSERT INTO t (b) VALUES (1);",
            )
            .unwrap();
        }
        for ro in [true, false] {
            let w = open(&path, ro);
            raw(&w, "SELECT a, b FROM t").unwrap();
            if !ro {
                raw(&w, "INSERT INTO t (b) VALUES (2)").unwrap();
                assert!(raw(&w, "INSERT INTO t (a, b) VALUES ('bad', 3)").is_err());
            }
        }
    }

    #[test]
    fn dates_and_timestamps() {
        assert_eq!(date_text(0).as_deref(), Some("1970-01-01"));
        assert_eq!(date_text(20_771).as_deref(), Some("2026-11-14"));
        assert_eq!(date_text(-719_528).as_deref(), Some("0000-01-01"));
        assert!(date_text(-719_529).is_none());
        assert_eq!(
            timestamp_text(1_000_001, true).as_deref(),
            Some("1970-01-01 00:00:01.000001Z")
        );
        assert_eq!(
            timestamp_text(-1, false).as_deref(),
            Some("1969-12-31 23:59:59.999999")
        );
        assert!(!params_supported(&[Param::Float(f64::NAN)]));
        assert!(params_supported(&[Param::Text("x".into()), Param::Null]));
    }

    #[test]
    fn immutable_uris_are_encoded() {
        assert_eq!(
            immutable_uri(Path::new("/tmp/a b/x?mode=rw&vfs=y#%.db")),
            "file:/tmp/a%20b/x%3Fmode%3Drw%26vfs%3Dy%23%25.db?mode=ro&immutable=1"
        );
        assert_eq!(
            immutable_uri(Path::new("/d/é.db")),
            "file:/d/%C3%A9.db?mode=ro&immutable=1"
        );
    }

    #[test]
    fn dml_keyword() {
        assert!(is_dml("  -- c\n/* x */ insert into t values (1)"));
        assert!(is_dml("REPLACE INTO t VALUES (1)"));
        assert!(!is_dml("CREATE TABLE t (a)"));
        assert!(!is_dml("WITH x AS (SELECT 1) DELETE FROM t"));
    }

    #[test]
    fn paths_are_checked() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            resolve_path(Path::new("relative.db"), false),
            Err(DriverError::InvalidParams(_))
        ));
        assert!(matches!(
            resolve_path(Path::new(":memory:"), false),
            Err(DriverError::InvalidParams(_))
        ));
        assert!(matches!(
            resolve_path(Path::new("file:x.db?mode=rwc"), true),
            Err(DriverError::InvalidParams(_))
        ));
        assert!(matches!(
            resolve_path(dir.path(), false),
            Err(DriverError::InvalidParams(_))
        ));
        let missing = dir.path().join("missing.db");
        assert!(matches!(
            resolve_path(&missing, false),
            Err(DriverError::Io(_))
        ));
        let created = resolve_path(&dir.path().join("sub/../new.db"), true);
        assert!(created.is_err(), "the parent `sub` does not exist");
        let created = resolve_path(&dir.path().join("new.db"), true).unwrap();
        assert!(created.is_absolute() && created.ends_with("new.db"));
        assert!(resolve_path(&dir.path().join(".."), true).is_err());
    }
}
