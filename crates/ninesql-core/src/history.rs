//! Query history (F-SQL-08, ADR-0008 §4, §6): what may be recorded, and a writer that never
//! blocks an execution. ROADMAP: P3-C. The file itself is
//! [`ninesql_storage::history::HistoryDb`] (`$XDG_STATE_HOME/ninesql/history.sqlite`, created
//! `0600`).
//!
//! # What is recorded (the logging rule, applied to history)
//! A statement is recorded only when **all** of these hold ([`may_record`]):
//! - history is enabled in the preferences ([`HistoryPreferences::enabled`]);
//! - the connection's effective type allows query text in logs
//!   ([`SafetyPolicy::log_query_text`]);
//! - the connection is not production ([`SafetyPolicy::production`]; production types never
//!   allow it anyway, ADR-0007 §6).
//!
//! - the statement text holds no credential word ([`has_credential_word`]): `ALTER ROLE …
//!   PASSWORD '…'`, `CREATE USER … IDENTIFIED BY …`, `SET PASSWORD`, `CREATE USER MAPPING …
//!   OPTIONS (password …)`, `CREATE SECRET`, `pgp_sym_encrypt(…)`, a URI with credentials
//!   (`dblink_connect('postgresql://u:pw@h/db')`, [`has_uri_credentials`]) and the like are
//!   never recorded (I1: no password in history), and their text is not logged either.
//!
//! The first three are the rule that decides whether query text may be logged (AGENTS.md,
//! ADR-0007 §6), and like ADR-0008 §4 says, nothing at all is stored otherwise: no redacted
//! entry, no timing, no row count. There is no redaction mode.
//!
//! # Credential words (fail closed)
//! [`has_credential_word`] splits the **whole** text into words of letters and digits (`_`,
//! quotes, dots and every other character separate words, so `aws_secret_access_key` holds
//! `secret`) and looks for [`CREDENTIAL_WORDS`], ignoring case. It deliberately looks inside
//! string literals, quoted identifiers and comments too, which is broader than SQL syntax
//! needs: dynamic SQL hides `ALTER ROLE … PASSWORD` inside a literal, and a password can sit in
//! a comment. A false positive (`SELECT password_hash …`, a column named `token`) only means
//! one history entry is missing; a false negative would put a secret on disk.
//!
//! When a statement is recorded, the entry holds its text exactly as it was sent, the
//! connection, project and engine ids, start time, duration, rows delivered, update count,
//! status, the stable error kind and engine error code (never the engine message, which may
//! quote values), and the **number** of bind parameters. **Bind parameter values are never
//! recorded** (I1): the model has no place for them. A secret typed as a literal in a statement
//! that has no credential word (an `INSERT` of a password into an application table) is not
//! detected: the editor's parameters prompt (F-SQL-06) binds such values separately.
//!
//! # Writer
//! One thread (`ninesql-history`) owns the database. Executions hand it entries through a
//! bounded queue without waiting: when the queue is full (a disk that does not keep up), the
//! entry is dropped and counted ([`History::dropped`]). Queries, deletes and pruning go through
//! the same queue and answer through a [`Reply`]. Retention ([`HistoryPreferences::retention`])
//! is applied at start, every [`PRUNE_EVERY`] entries and when it changes (after the inserts
//! queued before the change).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ninesql_storage::history::{EntryStatus, HistoryDb, HistoryEntry, HistoryQuery, NewEntry};
use ninesql_storage::{Retention, StorageError};
use tokio::sync::oneshot;

use crate::connection::{ConnectionId, EngineId};
use crate::connection_type::SafetyPolicy;
use crate::preferences::HistoryPreferences;
use crate::session::Reply;

/// Entries queued for the writer before new ones are dropped.
pub const HISTORY_QUEUE: usize = 1024;

/// Retention runs again after this many recorded entries.
pub const PRUNE_EVERY: u64 = 500;

/// Whether a statement of a connection with `policy` may be recorded under `prefs`, before
/// looking at its text ([`has_credential_word`]). See the module documentation.
#[must_use]
pub fn may_record(policy: &SafetyPolicy, prefs: &HistoryPreferences) -> bool {
    prefs.enabled && policy.log_query_text && !policy.production
}

/// The credential rule is shared with the drivers' statement logs, so the history and the logs
/// always skip the same statements: see [`ninesql_driver_api::hygiene`].
pub use ninesql_driver_api::hygiene::{CREDENTIAL_WORDS, has_uri_credentials};

/// Whether `text` holds a credential word or a URI with credentials anywhere: in keywords,
/// identifiers, string literals or comments (fail closed, see the module documentation). Such a
/// statement is never recorded in the history and its text is never logged. Same rule as
/// [`ninesql_driver_api::hygiene::text_may_hold_credentials`].
#[must_use]
pub fn has_credential_word(text: &str) -> bool {
    ninesql_driver_api::hygiene::text_may_hold_credentials(text)
}

/// Why the history could not answer.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum HistoryError {
    /// The database failed.
    #[error(transparent)]
    Storage(#[from] StorageError),
    /// The writer's queue is full; try again.
    #[error("the history is busy; try again")]
    Busy,
    /// The writer stopped.
    #[error("the history is closed")]
    Closed,
}

impl HistoryError {
    /// A short, stable name of the error kind, safe to log at any level: no path, no SQLite
    /// message (ADR-0029 §3.2).
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Storage(e) => e.kind(),
            Self::Busy => "history-busy",
            Self::Closed => "history-closed",
        }
    }
}

/// What became of a statement handed to [`History::record`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Recorded {
    /// Queued for writing.
    Queued,
    /// Not recorded: history is disabled in the preferences.
    Disabled,
    /// Not recorded: the connection type does not allow query text to be logged, or it is
    /// production.
    NotAllowed,
    /// Not recorded: the statement holds a credential word ([`has_credential_word`]).
    Sensitive,
    /// Not recorded: the writer's queue was full or the writer stopped.
    Dropped,
}

/// One finished statement, as the execution service reports it. There is deliberately no field
/// for parameter values (I1).
#[derive(Clone)]
#[non_exhaustive]
pub struct StatementRecord {
    /// Connection.
    pub connection: ConnectionId,
    /// Engine.
    pub engine: EngineId,
    /// Statement text as sent.
    pub text: String,
    /// When it started.
    pub started_at: SystemTime,
    /// How long it took.
    pub duration: Duration,
    /// Rows / documents delivered, when it returned a result.
    pub rows: Option<u64>,
    /// Rows affected, when reported.
    pub update_count: Option<u64>,
    /// How it ended.
    pub status: EntryStatus,
    /// Stable error kind ([`crate::CoreError::kind`]).
    pub error_kind: Option<&'static str>,
    /// Engine error code.
    pub error_code: Option<String>,
    /// Number of bind parameters.
    pub param_count: usize,
}

impl std::fmt::Debug for StatementRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StatementRecord")
            .field("connection", &self.connection)
            .field("text_len", &self.text.len())
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

impl StatementRecord {
    /// A record with no counts and no error.
    #[must_use]
    pub fn new(
        connection: ConnectionId,
        engine: EngineId,
        text: impl Into<String>,
        started_at: SystemTime,
        duration: Duration,
        status: EntryStatus,
    ) -> Self {
        Self {
            connection,
            engine,
            text: text.into(),
            started_at,
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

type Answer<T> = oneshot::Sender<Result<T, HistoryError>>;

enum Job {
    Insert(NewEntry),
    Query(HistoryQuery, Answer<Vec<HistoryEntry>>),
    Delete(Vec<i64>, Answer<u64>),
    DeleteConnection(String, Answer<u64>),
    Clear(Answer<u64>),
    Prune(Option<Answer<u64>>),
    Flush(Answer<()>),
    /// Wakes the writer to apply a retention change ([`WriterShared::retention_changed`]).
    Wake,
}

/// What the writer and the handles share.
struct WriterShared {
    retention: Mutex<Retention>,
    /// Set when the retention changed; the writer prunes at its next job, so a change is never
    /// lost when the queue is full (review L5).
    retention_changed: AtomicBool,
    /// Entries not written: dropped on a full queue, or failed to write (disk full…).
    dropped: AtomicU64,
}

struct Inner {
    jobs: SyncSender<Job>,
    project: String,
    enabled: AtomicBool,
    shared: Arc<WriterShared>,
}

/// The query history of the application. Cheap to clone; the writer thread stops when the last
/// clone is dropped.
#[derive(Clone)]
pub struct History {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for History {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("History")
            .field("project", &self.inner.project)
            .field("dropped", &self.dropped())
            .finish_non_exhaustive()
    }
}

fn unix_ms(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

fn now_ms() -> i64 {
    unix_ms(SystemTime::now())
}

impl History {
    /// Opens `db` (already opened on a blocking thread, [`HistoryDb::open`]) and starts its
    /// writer thread. `project` is the project id stored with each entry. Retention is applied
    /// once at start.
    ///
    /// # Errors
    /// [`HistoryError::Closed`] when the thread cannot start.
    pub fn start(
        db: HistoryDb,
        project: impl Into<String>,
        prefs: &HistoryPreferences,
    ) -> Result<Self, HistoryError> {
        let (jobs, rx) = mpsc::sync_channel(HISTORY_QUEUE);
        let shared = Arc::new(WriterShared {
            retention: Mutex::new(prefs.retention()),
            retention_changed: AtomicBool::new(false),
            dropped: AtomicU64::new(0),
        });
        let for_writer = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("ninesql-history".into())
            .spawn(move || writer(db, &rx, &for_writer))
            .map_err(|_| HistoryError::Closed)?;
        Ok(Self {
            inner: Arc::new(Inner {
                jobs,
                project: project.into(),
                enabled: AtomicBool::new(prefs.enabled),
                shared,
            }),
        })
    }

    /// Applies new preferences: enabled flag, and retention (pruned at once when it changed).
    pub fn set_preferences(&self, prefs: &HistoryPreferences) {
        self.inner.enabled.store(prefs.enabled, Ordering::Relaxed);
        let retention = prefs.retention();
        let changed = {
            let mut current = self
                .inner
                .shared
                .retention
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let changed = *current != retention;
            *current = retention;
            changed
        };
        if changed {
            self.inner
                .shared
                .retention_changed
                .store(true, Ordering::SeqCst);
            // The `Wake` queues behind the inserts sent before the change, so they are pruned
            // too. A full queue drops it: the writer then acts on the flag at the next job that
            // is not an insert, and the periodic prune meanwhile reads the new retention.
            let _ = self.inner.jobs.try_send(Job::Wake);
        }
    }

    /// Entries not written: dropped because the writer's queue was full, or whose write failed
    /// (disk full, damaged file).
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.inner.shared.dropped.load(Ordering::Relaxed)
    }

    /// Records a finished statement if [`may_record`] allows it; never blocks.
    pub fn record(&self, policy: &SafetyPolicy, record: StatementRecord) -> Recorded {
        if !self.inner.enabled.load(Ordering::Relaxed) {
            return Recorded::Disabled;
        }
        if !policy.log_query_text || policy.production {
            return Recorded::NotAllowed;
        }
        if has_credential_word(&record.text) {
            return Recorded::Sensitive;
        }
        let mut entry = NewEntry::new(
            unix_ms(record.started_at),
            self.inner.project.clone(),
            record.connection.to_string(),
            record.engine.as_str(),
            record.text,
            record.duration,
            record.status,
        );
        entry.rows = record.rows;
        entry.update_count = record.update_count;
        entry.error_kind = record.error_kind.map(str::to_owned);
        entry.error_code = record.error_code;
        entry.param_count = u32::try_from(record.param_count).unwrap_or(u32::MAX);
        match self.inner.jobs.try_send(Job::Insert(entry)) {
            Ok(()) => Recorded::Queued,
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                self.inner.shared.dropped.fetch_add(1, Ordering::Relaxed);
                tracing::debug!("history queue full: entry dropped");
                Recorded::Dropped
            }
        }
    }

    fn ask<T: Send + 'static>(
        &self,
        job: impl FnOnce(Answer<T>) -> Job,
    ) -> Reply<Result<T, HistoryError>> {
        let (tx, rx) = oneshot::channel();
        match self.inner.jobs.try_send(job(tx)) {
            Ok(()) => Reply::from_receiver(rx),
            Err(TrySendError::Full(_)) => Reply::ready(Err(HistoryError::Busy)),
            Err(TrySendError::Disconnected(_)) => Reply::ready(Err(HistoryError::Closed)),
        }
    }

    /// Entries matching `query`, newest first (the history panel, F-SQL-08). Never blocks.
    pub fn query(&self, query: HistoryQuery) -> Reply<Result<Vec<HistoryEntry>, HistoryError>> {
        self.ask(|tx| Job::Query(query, tx))
    }

    /// Entries of `connection` in this project, newest first.
    pub fn recent(
        &self,
        connection: ConnectionId,
        limit: u32,
    ) -> Reply<Result<Vec<HistoryEntry>, HistoryError>> {
        self.query(
            HistoryQuery::recent(limit)
                .for_project(self.inner.project.clone())
                .for_connection(connection.to_string()),
        )
    }

    /// Deletes entries by id.
    pub fn delete(&self, ids: Vec<i64>) -> Reply<Result<u64, HistoryError>> {
        self.ask(|tx| Job::Delete(ids, tx))
    }

    /// Deletes every entry of a connection (call it when a connection is deleted).
    pub fn delete_connection(&self, connection: ConnectionId) -> Reply<Result<u64, HistoryError>> {
        self.ask(|tx| Job::DeleteConnection(connection.to_string(), tx))
    }

    /// Deletes every entry.
    pub fn clear(&self) -> Reply<Result<u64, HistoryError>> {
        self.ask(Job::Clear)
    }

    /// Applies the retention now; answers how many entries were deleted.
    pub fn prune(&self) -> Reply<Result<u64, HistoryError>> {
        self.ask(|tx| Job::Prune(Some(tx)))
    }

    /// Answers once every entry queued before it is written (and the write-ahead log is
    /// checkpointed into the main file).
    pub fn flush(&self) -> Reply<Result<(), HistoryError>> {
        self.ask(Job::Flush)
    }
}

fn writer(mut db: HistoryDb, jobs: &mpsc::Receiver<Job>, shared: &WriterShared) {
    let retention = || {
        *shared
            .retention
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    };
    let prune = |db: &mut HistoryDb| match db.prune(&retention(), now_ms()) {
        Ok(n) => {
            if n > 0 {
                tracing::debug!(deleted = n, "history pruned");
            }
            Ok(n)
        }
        Err(e) => {
            tracing::warn!(kind = e.kind(), "history pruning failed");
            Err(HistoryError::from(e))
        }
    };
    let _ = prune(&mut db);
    let mut since_prune = 0u64;
    // A changed retention is applied at the `Wake` that `set_preferences` queues behind the
    // inserts sent before it, or before any job that reads or answers (when a full queue
    // dropped the `Wake`), never in the middle of earlier inserts: an insert queued before the
    // change and written after the prune would leave one entry over the new limit.
    let apply_changed_retention = |db: &mut HistoryDb, since_prune: &mut u64| {
        if shared.retention_changed.swap(false, Ordering::SeqCst) {
            *since_prune = 0;
            let _ = prune(db);
        }
    };
    while let Ok(job) = jobs.recv() {
        if !matches!(job, Job::Insert(_)) {
            apply_changed_retention(&mut db, &mut since_prune);
        }
        match job {
            Job::Insert(entry) => {
                if let Err(e) = db.insert(&entry) {
                    shared.dropped.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(kind = e.kind(), "history entry not written");
                }
                since_prune += 1;
                if since_prune >= PRUNE_EVERY {
                    since_prune = 0;
                    let _ = prune(&mut db);
                }
            }
            Job::Query(q, tx) => {
                let _ = tx.send(db.query(&q).map_err(HistoryError::from));
            }
            Job::Delete(ids, tx) => {
                let _ = tx.send(db.delete(&ids).map_err(HistoryError::from));
            }
            Job::DeleteConnection(c, tx) => {
                let _ = tx.send(db.delete_connection(&c).map_err(HistoryError::from));
            }
            Job::Clear(tx) => {
                let _ = tx.send(db.clear().map_err(HistoryError::from));
            }
            Job::Prune(tx) => {
                since_prune = 0;
                let result = prune(&mut db);
                if let Some(tx) = tx {
                    let _ = tx.send(result);
                }
            }
            Job::Flush(tx) => {
                let _ = tx.send(db.checkpoint().map_err(HistoryError::from));
            }
            Job::Wake => {}
        }
    }
    let _ = db.checkpoint();
    tracing::debug!("history writer stopped");
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).
mod tests {
    use super::*;
    use crate::connection_type::ConnectionType;
    use ninesql_storage::history::HistoryQuery;

    #[test]
    fn credential_words_are_found_anywhere_ignoring_case() {
        for text in [
            "ALTER ROLE bob PASSWORD 'x'",
            "alter role bob with encrypted password 'x'",
            "CREATE USER bob IDENTIFIED BY 'x'",
            "ALTER USER bob IDENTIFIED WITH mysql_native_password BY 'x'",
            "SET PASSWORD FOR bob = 'x'",
            "CREATE USER MAPPING FOR bob SERVER s OPTIONS (user 'u', password 'x')",
            "CREATE SECRET s (TYPE s3, KEY_ID 'k', SECRET 'x')",
            "SELECT pgp_sym_encrypt('data', 'x')",
            "EXECUTE 'ALTER ROLE bob PASSWORD ' || quote_literal(p)",
            "SELECT 1 -- password: hunter2",
            "SELECT \"Password\" FROM t",
            "UPDATE s SET aws_secret_access_key = 'x'",
            "ALTER ROLE bob PASSWORD$$x$$",
            "SELECT dblink_connect('postgresql://u:pw@h/db')",
            "CREATE SUBSCRIPTION s CONNECTION 'postgres://repl:s3cr3t@primary:5432/app' PUBLICATION p",
            "SELECT * FROM t WHERE url = 'mysql://root:@localhost'",
            "SELECT 'https://u:p/q@host'",
        ] {
            assert!(has_credential_word(text), "{text}");
        }
        for text in [
            "SELECT * FROM numbers(3)",
            "DELETE FROM t WHERE id = 1",
            "SELECT passwordless FROM t",
            "CREATE TABLE tokens (id int)",
            "SELECT 'https://example.test/a@b'",
            "SELECT 'postgresql://host:5432/db'",
            "SELECT 'mailto:user@example.test'",
            "SELECT 'a://b c:d@e'",
            "",
        ] {
            assert!(!has_credential_word(text), "{text}");
        }
    }

    /// Review L5: a write that fails is counted, and a retention change is applied.
    #[test]
    fn failed_writes_are_counted_and_retention_changes_apply() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("h.sqlite");
        let db = HistoryDb::open(&path).unwrap();
        let prefs = HistoryPreferences::default();
        let history = History::start(db, "p", &prefs).unwrap();
        let policy = ConnectionType::development().policy();
        let record = |text: &str| {
            StatementRecord::new(
                ConnectionId::new_random(),
                EngineId::new("mock").unwrap(),
                text,
                SystemTime::now(),
                Duration::from_millis(1),
                EntryStatus::Succeeded,
            )
        };
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            assert_eq!(
                history.record(&policy, record("SELECT 1")),
                Recorded::Queued
            );
            assert_eq!(
                history.record(&policy, record("SELECT 2")),
                Recorded::Queued
            );
            history.flush().await.unwrap().unwrap();
            // Keep no entry per connection: the change prunes both.
            history.set_preferences(&HistoryPreferences {
                max_entries_per_connection: 0,
                ..prefs
            });
            history.flush().await.unwrap().unwrap();
            let left = history
                .query(HistoryQuery::recent(10))
                .await
                .unwrap()
                .unwrap();
            assert!(left.is_empty(), "retention change applied");
            assert_eq!(history.dropped(), 0);
            // The table disappears under the writer (a damaged file): the write fails.
            rusqlite::Connection::open(&path)
                .unwrap()
                .execute_batch("DROP TABLE entries")
                .unwrap();
            history.record(&policy, record("SELECT 3"));
            let _ = history.flush().await;
            assert_eq!(history.dropped(), 1, "failed write counted");
        });
    }

    #[test]
    fn the_logging_rule_decides() {
        let prefs = HistoryPreferences::default();
        assert!(may_record(&ConnectionType::development().policy(), &prefs));
        assert!(!may_record(&ConnectionType::production().policy(), &prefs));
        let mut quiet = ConnectionType::test();
        quiet.log_query_text = false;
        assert!(!may_record(&quiet.policy(), &prefs));
        let off = HistoryPreferences {
            enabled: false,
            ..prefs
        };
        assert!(!may_record(&ConnectionType::development().policy(), &off));
        // Even a production policy that claims to allow logging is refused.
        let mut forged = ConnectionType::production().policy();
        forged.log_query_text = true;
        assert!(!may_record(&forged, &prefs));
    }
}
