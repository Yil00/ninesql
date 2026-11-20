//! Query history database: `$XDG_STATE_HOME/ninesql/history.sqlite` (ADR-0008 §1, §4, §6;
//! F-SQL-08). ROADMAP: P3-C.
//!
//! This module stores what it is given and knows nothing about connection types: the rules that
//! decide **whether** a statement may be recorded (never for a type with `log_query_text = false`,
//! never for production, never a bind parameter value) live in `ninesql-core`'s history recorder.
//! The model has no column for parameter values at all, only their count (I1).
//!
//! # The file
//! - **Ours, private.** The state directory is created `0700` ([`fs::ensure_private_dir`]); the
//!   file is created by NineSQL itself with mode `0600` (`O_CREAT | O_EXCL | O_NOFOLLOW`) before
//!   SQLite opens it, so it never exists with wider permissions. An existing file must be a
//!   regular file owned by the current user (a symbolic link is refused, never followed); group /
//!   other bits are removed. SQLite is opened with `SQLITE_OPEN_NOFOLLOW` and without
//!   `SQLITE_OPEN_CREATE`. SQLite gives its `-wal` / `-shm` files the database file's mode; any
//!   that already exist are checked like the file itself.
//! - **Versioned.** The schema version is SQLite's `user_version`; a newer one is refused
//!   ([`StorageError::NewerVersion`]) and left untouched; every bump adds a step to
//!   [`MIGRATIONS`].
//! - **Hygiene.** `secure_delete` is on: a deleted entry is overwritten in the database pages.
//!   In WAL mode the write-ahead log still holds older images of those pages until a
//!   checkpoint, so every delete ([`HistoryDb::delete`], [`HistoryDb::delete_connection`],
//!   [`HistoryDb::clear`], a [`HistoryDb::prune`] that removed entries) is followed by a
//!   `wal_checkpoint(TRUNCATE)`, and `journal_size_limit = 0` truncates the log after every
//!   checkpoint. Residual: a checkpoint that another connection blocks (a second NineSQL
//!   process reading the file) leaves the old images in the log until the next one. Temporary
//!   tables stay in memory; `trusted_schema` is off.
//! - **Bounded.** A statement longer than [`MAX_STATEMENT_BYTES`] is stored cut (flagged); a
//!   query returns at most [`MAX_QUERY_LIMIT`] entries.

use std::fs::OpenOptions;
use std::io;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

use crate::error::StorageError;
use crate::fs::{self, FILE_MODE};

/// Current schema version (`PRAGMA user_version`).
pub const HISTORY_SCHEMA_VERSION: u32 = 1;

/// Longest statement text stored, in bytes; longer ones are cut at a `char` boundary and
/// flagged ([`HistoryEntry::truncated`]).
pub const MAX_STATEMENT_BYTES: usize = 1024 * 1024;

/// Most entries one [`HistoryDb::query`] returns.
pub const MAX_QUERY_LIMIT: u32 = 10_000;

/// Default retention (ADR-0008 §6): entries kept per connection.
pub const DEFAULT_MAX_ENTRIES_PER_CONNECTION: u32 = 10_000;

/// Default retention (ADR-0008 §6): entries older than this are deleted.
pub const DEFAULT_MAX_AGE: Duration = Duration::from_secs(90 * 24 * 60 * 60);

/// How long a writer waits for another connection's lock.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

const SCHEMA_V1: &str = "
CREATE TABLE entries (
    id            INTEGER PRIMARY KEY,
    recorded_at   INTEGER NOT NULL,
    project       TEXT    NOT NULL,
    connection    TEXT    NOT NULL,
    engine        TEXT    NOT NULL,
    statement     TEXT    NOT NULL,
    truncated     INTEGER NOT NULL DEFAULT 0,
    duration_ms   INTEGER NOT NULL,
    rows          INTEGER,
    update_count  INTEGER,
    status        TEXT    NOT NULL,
    error_kind    TEXT,
    error_code    TEXT,
    param_count   INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX entries_by_connection ON entries (connection, id);
CREATE INDEX entries_by_time ON entries (recorded_at);
";

/// One schema step from `from` to `from + 1`.
struct Step {
    from: u32,
    sql: &'static str,
}

/// Schema steps; version 0 is an empty file.
const MIGRATIONS: &[Step] = &[Step {
    from: 0,
    sql: SCHEMA_V1,
}];

/// How a recorded statement ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum EntryStatus {
    /// Ran to the end.
    Succeeded,
    /// The engine or the driver reported an error.
    Failed,
    /// Stopped by the user.
    Cancelled,
    /// Stopped by its timeout.
    TimedOut,
}

impl EntryStatus {
    /// Stable name stored in the file.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "ok",
            Self::Failed => "error",
            Self::Cancelled => "cancelled",
            Self::TimedOut => "timeout",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "ok" => Self::Succeeded,
            "cancelled" => Self::Cancelled,
            "timeout" => Self::TimedOut,
            _ => Self::Failed,
        }
    }
}

/// A statement to record. There is deliberately no field for parameter values (I1).
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct NewEntry {
    /// When the statement started, Unix time in milliseconds.
    pub recorded_at_ms: i64,
    /// Project id.
    pub project: String,
    /// Connection id.
    pub connection: String,
    /// Engine id.
    pub engine: String,
    /// Statement text, exactly as sent (cut at [`MAX_STATEMENT_BYTES`]).
    pub statement: String,
    /// Wall-clock duration.
    pub duration: Duration,
    /// Rows / documents delivered, when the statement returned a result.
    pub rows: Option<u64>,
    /// Rows affected, when the engine reported a count.
    pub update_count: Option<u64>,
    /// How it ended.
    pub status: EntryStatus,
    /// Stable error kind (`driver-query`…), never the engine message (it may quote values).
    pub error_kind: Option<String>,
    /// Engine error code (SQLSTATE, MySQL error number).
    pub error_code: Option<String>,
    /// Number of bind parameters (never their values).
    pub param_count: u32,
}

impl std::fmt::Debug for NewEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NewEntry")
            .field("connection", &self.connection)
            .field("statement_len", &self.statement.len())
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

impl NewEntry {
    /// An entry with the required fields; counts, errors and parameters empty.
    #[must_use]
    pub fn new(
        recorded_at_ms: i64,
        project: impl Into<String>,
        connection: impl Into<String>,
        engine: impl Into<String>,
        statement: impl Into<String>,
        duration: Duration,
        status: EntryStatus,
    ) -> Self {
        Self {
            recorded_at_ms,
            project: project.into(),
            connection: connection.into(),
            engine: engine.into(),
            statement: statement.into(),
            duration,
            rows: None,
            update_count: None,
            status,
            error_kind: None,
            error_code: None,
            param_count: 0,
        }
    }
}

/// A recorded statement.
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct HistoryEntry {
    /// Row id (increasing with time).
    pub id: i64,
    /// When the statement started, Unix time in milliseconds.
    pub recorded_at_ms: i64,
    /// Project id.
    pub project: String,
    /// Connection id.
    pub connection: String,
    /// Engine id.
    pub engine: String,
    /// Statement text.
    pub statement: String,
    /// The text was cut at [`MAX_STATEMENT_BYTES`].
    pub truncated: bool,
    /// Wall-clock duration.
    pub duration: Duration,
    /// Rows / documents delivered.
    pub rows: Option<u64>,
    /// Rows affected.
    pub update_count: Option<u64>,
    /// How it ended.
    pub status: EntryStatus,
    /// Stable error kind.
    pub error_kind: Option<String>,
    /// Engine error code.
    pub error_code: Option<String>,
    /// Number of bind parameters.
    pub param_count: u32,
}

impl std::fmt::Debug for HistoryEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HistoryEntry")
            .field("id", &self.id)
            .field("connection", &self.connection)
            .field("statement_len", &self.statement.len())
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

/// Filter of [`HistoryDb::query`]. Newest first.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct HistoryQuery {
    /// Only this project.
    pub project: Option<String>,
    /// Only this connection.
    pub connection: Option<String>,
    /// Only statements containing this text (case-sensitive substring, no wildcard).
    pub contains: Option<String>,
    /// Only entries with an id below this one (paging: pass the last id of the previous page).
    pub before_id: Option<i64>,
    /// At most this many entries (clamped to `1..=MAX_QUERY_LIMIT`).
    pub limit: u32,
}

impl Default for HistoryQuery {
    fn default() -> Self {
        Self {
            project: None,
            connection: None,
            contains: None,
            before_id: None,
            limit: 200,
        }
    }
}

impl HistoryQuery {
    /// The newest `limit` entries of every connection.
    #[must_use]
    pub fn recent(limit: u32) -> Self {
        Self {
            limit,
            ..Self::default()
        }
    }

    /// Only `connection`.
    #[must_use]
    pub fn for_connection(mut self, connection: impl Into<String>) -> Self {
        self.connection = Some(connection.into());
        self
    }

    /// Only `project`.
    #[must_use]
    pub fn for_project(mut self, project: impl Into<String>) -> Self {
        self.project = Some(project.into());
        self
    }

    /// Only statements containing `text`.
    #[must_use]
    pub fn containing(mut self, text: impl Into<String>) -> Self {
        self.contains = Some(text.into());
        self
    }

    /// Only entries older than `id`.
    #[must_use]
    pub fn before(mut self, id: i64) -> Self {
        self.before_id = Some(id);
        self
    }
}

/// Retention rules (ADR-0008 §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retention {
    /// Entries kept per connection (the newest); `0` keeps none.
    pub max_entries_per_connection: u32,
    /// Entries older than this are deleted; `None`: no age limit.
    pub max_age: Option<Duration>,
}

impl Default for Retention {
    fn default() -> Self {
        Self {
            max_entries_per_connection: DEFAULT_MAX_ENTRIES_PER_CONNECTION,
            max_age: Some(DEFAULT_MAX_AGE),
        }
    }
}

/// An open history database. Blocking: use it from a dedicated or blocking thread.
pub struct HistoryDb {
    conn: Connection,
    path: PathBuf,
}

impl std::fmt::Debug for HistoryDb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HistoryDb")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

fn sql_error(path: &Path, e: &rusqlite::Error) -> StorageError {
    StorageError::History {
        path: path.to_path_buf(),
        detail: e.to_string(),
    }
}

fn to_i64(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn to_u64(v: Option<i64>) -> Option<u64> {
    v.map(|v| u64::try_from(v).unwrap_or(0))
}

/// Creates `path` with mode `0600` if it does not exist, never through a symbolic link; checks
/// an existing one is a regular file of ours and tightens its mode.
fn create_or_check_private_file(path: &Path) -> Result<(), StorageError> {
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(FILE_MODE)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
    {
        Ok(file) => {
            drop(file);
            Ok(())
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => check_existing(path),
        Err(e) => Err(StorageError::Io {
            path: path.to_path_buf(),
            source: e,
        }),
    }
}

fn check_existing(path: &Path) -> Result<(), StorageError> {
    let meta = std::fs::symlink_metadata(path).map_err(|e| StorageError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    if !meta.file_type().is_file() {
        return Err(StorageError::WrongFileType {
            path: path.to_path_buf(),
            expected: "regular file (symbolic links, FIFOs and devices are refused)",
        });
    }
    fs::check_owner(path, meta.uid(), fs::current_uid())?;
    fs::ensure_private_file(path)
}

/// The SQLite side files of `path` (`-wal`, `-shm`, `-journal`).
fn side_files(path: &Path) -> [PathBuf; 3] {
    let with = |suffix: &str| {
        let mut s = path.as_os_str().to_owned();
        s.push(suffix);
        PathBuf::from(s)
    };
    [with("-wal"), with("-shm"), with("-journal")]
}

impl HistoryDb {
    /// Opens (creating it if needed) the history database at `path`, usually
    /// [`crate::Paths::history_file`], and brings its schema up to date. **Blocking.**
    ///
    /// # Errors
    /// [`StorageError::Io`], [`StorageError::WrongFileType`], [`StorageError::NotOwned`] for the
    /// directory or the file; [`StorageError::NewerVersion`] when a newer NineSQL wrote it;
    /// [`StorageError::History`] for SQLite errors (a damaged file).
    pub fn open(path: &Path) -> Result<Self, StorageError> {
        if let Some(dir) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::ensure_private_dir(dir)?;
        }
        create_or_check_private_file(path)?;
        for side in side_files(path) {
            if std::fs::symlink_metadata(&side).is_ok() {
                check_existing(&side)?;
            }
        }
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW
            | OpenFlags::SQLITE_OPEN_EXRESCODE;
        let conn = Connection::open_with_flags(path, flags).map_err(|e| sql_error(path, &e))?;
        let mut db = Self {
            conn,
            path: path.to_path_buf(),
        };
        db.configure()?;
        db.migrate()?;
        Ok(db)
    }

    fn configure(&self) -> Result<(), StorageError> {
        let path = &self.path;
        self.conn
            .busy_timeout(BUSY_TIMEOUT)
            .map_err(|e| sql_error(path, &e))?;
        // `journal_mode` answers with a row.
        let _mode: String = self
            .conn
            .query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))
            .map_err(|e| sql_error(path, &e))?;
        for pragma in [
            "PRAGMA synchronous = NORMAL",
            "PRAGMA secure_delete = ON",
            "PRAGMA journal_size_limit = 0",
            "PRAGMA temp_store = MEMORY",
            "PRAGMA trusted_schema = OFF",
        ] {
            self.conn
                .execute_batch(pragma)
                .map_err(|e| sql_error(path, &e))?;
        }
        Ok(())
    }

    fn version(&self) -> Result<u32, StorageError> {
        let v: i64 = self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .map_err(|e| sql_error(&self.path, &e))?;
        Ok(u32::try_from(v).unwrap_or(u32::MAX))
    }

    fn migrate(&mut self) -> Result<(), StorageError> {
        let mut version = self.version()?;
        if version > HISTORY_SCHEMA_VERSION {
            return Err(StorageError::NewerVersion {
                path: self.path.clone(),
                found: version,
                supported: HISTORY_SCHEMA_VERSION,
            });
        }
        while version < HISTORY_SCHEMA_VERSION {
            let step = MIGRATIONS.iter().find(|s| s.from == version).ok_or(
                StorageError::MissingMigration {
                    document: "history",
                    from: version,
                },
            )?;
            let path = self.path.clone();
            let tx = self.conn.transaction().map_err(|e| sql_error(&path, &e))?;
            tx.execute_batch(step.sql)
                .and_then(|()| tx.pragma_update(None, "user_version", version + 1))
                .and_then(|()| tx.commit())
                .map_err(|e| StorageError::Migration {
                    path: path.clone(),
                    from: version,
                    detail: e.to_string(),
                })?;
            if version > 0 {
                tracing::info!(file = %crate::fs::log_name(&path), from = version, "migrated the history database");
            }
            version += 1;
        }
        Ok(())
    }

    /// Path of the file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Records one statement; returns its id. **Blocking.**
    ///
    /// # Errors
    /// [`StorageError::History`].
    pub fn insert(&mut self, entry: &NewEntry) -> Result<i64, StorageError> {
        let (text, truncated) = cut(&entry.statement, MAX_STATEMENT_BYTES);
        self.conn
            .execute(
                "INSERT INTO entries (recorded_at, project, connection, engine, statement, \
                 truncated, duration_ms, rows, update_count, status, error_kind, error_code, \
                 param_count) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                params![
                    entry.recorded_at_ms,
                    entry.project,
                    entry.connection,
                    entry.engine,
                    text,
                    truncated,
                    to_i64(u64::try_from(entry.duration.as_millis()).unwrap_or(u64::MAX)),
                    entry.rows.map(to_i64),
                    entry.update_count.map(to_i64),
                    entry.status.as_str(),
                    entry.error_kind,
                    entry.error_code,
                    entry.param_count,
                ],
            )
            .map_err(|e| sql_error(&self.path, &e))?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Entries matching `query`, newest first. **Blocking.**
    ///
    /// # Errors
    /// [`StorageError::History`].
    pub fn query(&self, query: &HistoryQuery) -> Result<Vec<HistoryEntry>, StorageError> {
        let limit = query.limit.clamp(1, MAX_QUERY_LIMIT);
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, recorded_at, project, connection, engine, statement, truncated, \
                 duration_ms, rows, update_count, status, error_kind, error_code, param_count \
                 FROM entries \
                 WHERE (?1 IS NULL OR project = ?1) \
                   AND (?2 IS NULL OR connection = ?2) \
                   AND (?3 IS NULL OR instr(statement, ?3) > 0) \
                   AND (?4 IS NULL OR id < ?4) \
                 ORDER BY id DESC LIMIT ?5",
            )
            .map_err(|e| sql_error(&self.path, &e))?;
        let rows = stmt
            .query_map(
                params![
                    query.project,
                    query.connection,
                    query.contains,
                    query.before_id,
                    limit
                ],
                |r| {
                    let duration_ms: i64 = r.get(7)?;
                    let status: String = r.get(10)?;
                    let param_count: i64 = r.get(13)?;
                    Ok(HistoryEntry {
                        id: r.get(0)?,
                        recorded_at_ms: r.get(1)?,
                        project: r.get(2)?,
                        connection: r.get(3)?,
                        engine: r.get(4)?,
                        statement: r.get(5)?,
                        truncated: r.get(6)?,
                        duration: Duration::from_millis(u64::try_from(duration_ms).unwrap_or(0)),
                        rows: to_u64(r.get(8)?),
                        update_count: to_u64(r.get(9)?),
                        status: EntryStatus::parse(&status),
                        error_kind: r.get(11)?,
                        error_code: r.get(12)?,
                        param_count: u32::try_from(param_count).unwrap_or(0),
                    })
                },
            )
            .map_err(|e| sql_error(&self.path, &e))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| sql_error(&self.path, &e))
    }

    /// Number of entries. **Blocking.**
    ///
    /// # Errors
    /// [`StorageError::History`].
    pub fn count(&self) -> Result<u64, StorageError> {
        let n: i64 = self
            .conn
            .query_row("SELECT count(*) FROM entries", [], |r| r.get(0))
            .map_err(|e| sql_error(&self.path, &e))?;
        Ok(u64::try_from(n).unwrap_or(0))
    }

    /// Applies `retention` at `now_ms` (Unix milliseconds); returns how many entries were
    /// deleted. **Blocking.**
    ///
    /// # Errors
    /// [`StorageError::History`].
    pub fn prune(&mut self, retention: &Retention, now_ms: i64) -> Result<u64, StorageError> {
        let path = self.path.clone();
        let tx = self.conn.transaction().map_err(|e| sql_error(&path, &e))?;
        let mut deleted = 0usize;
        if let Some(age) = retention.max_age {
            let cutoff = now_ms.saturating_sub(i64::try_from(age.as_millis()).unwrap_or(i64::MAX));
            deleted += tx
                .execute("DELETE FROM entries WHERE recorded_at < ?1", [cutoff])
                .map_err(|e| sql_error(&path, &e))?;
        }
        deleted += tx
            .execute(
                "DELETE FROM entries WHERE id IN (SELECT id FROM (SELECT id, row_number() OVER \
                 (PARTITION BY connection ORDER BY id DESC) AS n FROM entries) WHERE n > ?1)",
                [retention.max_entries_per_connection],
            )
            .map_err(|e| sql_error(&path, &e))?;
        tx.commit().map_err(|e| sql_error(&path, &e))?;
        if deleted > 0 {
            self.checkpoint()?;
        }
        Ok(deleted as u64)
    }

    /// Deletes the entries with these ids. **Blocking.**
    ///
    /// # Errors
    /// [`StorageError::History`].
    pub fn delete(&mut self, ids: &[i64]) -> Result<u64, StorageError> {
        let path = self.path.clone();
        let tx = self.conn.transaction().map_err(|e| sql_error(&path, &e))?;
        let mut deleted = 0usize;
        for id in ids {
            deleted += tx
                .execute("DELETE FROM entries WHERE id = ?1", [id])
                .map_err(|e| sql_error(&path, &e))?;
        }
        tx.commit().map_err(|e| sql_error(&path, &e))?;
        self.checkpoint()?;
        Ok(deleted as u64)
    }

    /// Deletes every entry of a connection (a deleted connection, "clear history").
    /// **Blocking.**
    ///
    /// # Errors
    /// [`StorageError::History`].
    pub fn delete_connection(&mut self, connection: &str) -> Result<u64, StorageError> {
        let n = self
            .conn
            .execute("DELETE FROM entries WHERE connection = ?1", [connection])
            .map_err(|e| sql_error(&self.path, &e))?;
        self.checkpoint()?;
        Ok(n as u64)
    }

    /// Deletes every entry. **Blocking.**
    ///
    /// # Errors
    /// [`StorageError::History`].
    pub fn clear(&mut self) -> Result<u64, StorageError> {
        let n = self
            .conn
            .execute("DELETE FROM entries", [])
            .map_err(|e| sql_error(&self.path, &e))?;
        self.checkpoint()?;
        Ok(n as u64)
    }

    /// Writes the write-ahead log back into the main file and truncates it. Run after every
    /// delete, so older page images of deleted entries do not stay in the log. **Blocking.**
    ///
    /// # Errors
    /// [`StorageError::History`].
    pub fn checkpoint(&self) -> Result<(), StorageError> {
        let _: Option<i64> = self
            .conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get(0))
            .optional()
            .map_err(|e| sql_error(&self.path, &e))?;
        Ok(())
    }
}

/// `text` cut to at most `max` bytes at a `char` boundary, and whether it was cut.
fn cut(text: &str, max: usize) -> (&str, bool) {
    if text.len() <= max {
        return (text, false);
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], true)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn entry(connection: &str, at: i64, text: &str) -> NewEntry {
        NewEntry::new(
            at,
            "p",
            connection,
            "mock",
            text,
            Duration::from_millis(12),
            EntryStatus::Succeeded,
        )
    }

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn creates_a_private_file_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state/ninesql/history.sqlite");
        let mut db = HistoryDb::open(&path).unwrap();
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        let mut e = entry("c1", 1_000, "SELECT 1");
        e.rows = Some(1);
        e.param_count = 2;
        let id = db.insert(&e).unwrap();
        let mut f = entry("c1", 2_000, "DELETE FROM t");
        f.status = EntryStatus::Failed;
        f.error_kind = Some("driver-query".into());
        f.error_code = Some("42P01".into());
        db.insert(&f).unwrap();
        for side in side_files(&path) {
            if side.exists() {
                assert_eq!(mode(&side), 0o600, "{}", side.display());
            }
        }
        let all = db.query(&HistoryQuery::recent(10)).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].statement, "DELETE FROM t", "newest first");
        assert_eq!(all[0].status, EntryStatus::Failed);
        assert_eq!(all[0].error_code.as_deref(), Some("42P01"));
        assert_eq!(all[1].id, id);
        assert_eq!(all[1].rows, Some(1));
        assert_eq!(all[1].param_count, 2);
        assert_eq!(all[1].duration, Duration::from_millis(12));
        let found = db
            .query(&HistoryQuery::recent(10).containing("DELETE"))
            .unwrap();
        assert_eq!(found.len(), 1);
        let page = db
            .query(&HistoryQuery::recent(10).before(all[0].id))
            .unwrap();
        assert_eq!(page.len(), 1);
        drop(db);
        // Reopening keeps the data and the version.
        let db = HistoryDb::open(&path).unwrap();
        assert_eq!(db.count().unwrap(), 2);
        assert_eq!(db.version().unwrap(), HISTORY_SCHEMA_VERSION);
    }

    #[test]
    fn refuses_symlinks_and_tightens_modes() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("elsewhere.sqlite");
        std::fs::write(&target, b"").unwrap();
        let link = dir.path().join("history.sqlite");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(matches!(
            HistoryDb::open(&link),
            Err(StorageError::WrongFileType { .. })
        ));

        let path = dir.path().join("loose.sqlite");
        std::fs::write(&path, b"").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        HistoryDb::open(&path).unwrap();
        assert_eq!(mode(&path), 0o600);
    }

    #[test]
    fn refuses_a_newer_schema() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.sqlite");
        drop(HistoryDb::open(&path).unwrap());
        {
            let c = Connection::open(&path).unwrap();
            c.pragma_update(None, "user_version", 99).unwrap();
        }
        assert!(matches!(
            HistoryDb::open(&path),
            Err(StorageError::NewerVersion { found: 99, .. })
        ));
    }

    #[test]
    fn retention_by_count_and_age() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = HistoryDb::open(&dir.path().join("h.sqlite")).unwrap();
        for i in 0..10 {
            db.insert(&entry("a", 1_000 + i, &format!("SELECT {i}")))
                .unwrap();
        }
        for i in 0..3 {
            db.insert(&entry("b", 100 + i, "SELECT b")).unwrap();
        }
        let retention = Retention {
            max_entries_per_connection: 4,
            max_age: Some(Duration::from_millis(500)),
        };
        // now = 1_200: entries before 700 (all of `b`) are too old; `a` keeps its newest 4.
        let deleted = db.prune(&retention, 1_200).unwrap();
        assert_eq!(deleted, 3 + 6);
        let left = db.query(&HistoryQuery::recent(100)).unwrap();
        assert_eq!(left.len(), 4);
        assert!(left.iter().all(|e| e.connection == "a"));
        assert_eq!(left[0].statement, "SELECT 9");
        assert_eq!(db.delete(&[left[0].id]).unwrap(), 1);
        assert_eq!(db.delete_connection("a").unwrap(), 3);
        db.insert(&entry("c", 1, "x")).unwrap();
        assert_eq!(db.clear().unwrap(), 1);
        assert_eq!(db.count().unwrap(), 0);
    }

    /// Review L1: deleted text is gone from the main file and from the write-ahead log.
    #[test]
    fn deleted_text_leaves_the_file_and_the_log() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("h.sqlite");
        let mut db = HistoryDb::open(&path).unwrap();
        let bytes = |path: &Path| {
            let mut all = std::fs::read(path).unwrap();
            for side in side_files(path) {
                if let Ok(b) = std::fs::read(&side) {
                    all.extend(b);
                }
            }
            all
        };
        let has =
            |hay: &[u8], needle: &str| hay.windows(needle.len()).any(|w| w == needle.as_bytes());
        // One entry checkpointed into the main file, one still in the log only.
        let a = db
            .insert(&entry("c", 1, "SELECT 'marker-in-main-file'"))
            .unwrap();
        db.checkpoint().unwrap();
        let b = db
            .insert(&entry("c", 2, "SELECT 'marker-in-the-log'"))
            .unwrap();
        db.insert(&entry("d", 3, "SELECT 'marker-cleared'"))
            .unwrap();
        let before = bytes(&path);
        assert!(has(&before, "marker-in-main-file") && has(&before, "marker-in-the-log"));
        assert_eq!(db.delete(&[a, b]).unwrap(), 2);
        let after = bytes(&path);
        assert!(
            !has(&after, "marker-in-main-file"),
            "deleted text left in the file"
        );
        assert!(
            !has(&after, "marker-in-the-log"),
            "deleted text left in the log"
        );
        assert!(has(&after, "marker-cleared"));
        assert_eq!(db.delete_connection("d").unwrap(), 1);
        assert!(!has(&bytes(&path), "marker-cleared"));
        db.insert(&entry("e", 4, "SELECT 'marker-clear-all'"))
            .unwrap();
        db.clear().unwrap();
        db.insert(&entry("f", 1, "SELECT 'marker-pruned'")).unwrap();
        db.prune(
            &Retention {
                max_entries_per_connection: 10,
                max_age: Some(Duration::from_millis(1)),
            },
            1_000,
        )
        .unwrap();
        let after = bytes(&path);
        assert!(!has(&after, "marker-clear-all"));
        assert!(!has(&after, "marker-pruned"));
    }

    #[test]
    fn long_statements_are_cut_at_a_char_boundary() {
        let (s, cut_) = cut("ééé", 3);
        assert_eq!(s, "é");
        assert!(cut_);
        assert_eq!(cut("abc", 3), ("abc", false));
        let dir = tempfile::tempdir().unwrap();
        let mut db = HistoryDb::open(&dir.path().join("h.sqlite")).unwrap();
        db.insert(&entry("a", 1, &"x".repeat(MAX_STATEMENT_BYTES + 10)))
            .unwrap();
        let e = &db.query(&HistoryQuery::recent(1)).unwrap()[0];
        assert!(e.truncated);
        assert_eq!(e.statement.len(), MAX_STATEMENT_BYTES);
    }
}
