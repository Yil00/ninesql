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
//! This is the rule that decides whether query text may be logged (AGENTS.md, ADR-0007 §6), and
//! like ADR-0008 §4 says, nothing at all is stored otherwise: no redacted entry, no timing, no
//! row count. There is no redaction mode.
//!
//! When a statement is recorded, the entry holds its text exactly as it was sent, the
//! connection, project and engine ids, start time, duration, rows delivered, update count,
//! status, the stable error kind and engine error code (never the engine message, which may
//! quote values), and the **number** of bind parameters. **Bind parameter values are never
//! recorded** (I1): the model has no place for them. A secret typed as a literal inside the
//! statement text is not detected or removed: it is part of the text the user chose to run on a
//! connection that allows its queries to be logged (the editor's parameters prompt, F-SQL-06,
//! binds values separately).
//!
//! # Writer
//! One thread (`ninesql-history`) owns the database. Executions hand it entries through a
//! bounded queue without waiting: when the queue is full (a disk that does not keep up), the
//! entry is dropped and counted ([`History::dropped`]). Queries, deletes and pruning go through
//! the same queue and answer through a [`Reply`]. Retention ([`HistoryPreferences::retention`])
//! is applied at start, every [`PRUNE_EVERY`] entries and when it changes.

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

/// Whether a statement of a connection with `policy` may be recorded under `prefs`. See the
/// module documentation.
#[must_use]
pub fn may_record(policy: &SafetyPolicy, prefs: &HistoryPreferences) -> bool {
    prefs.enabled && policy.log_query_text && !policy.production
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
    Prune(Retention, Option<Answer<u64>>),
    Flush(Answer<()>),
}

struct Inner {
    jobs: SyncSender<Job>,
    project: String,
    enabled: AtomicBool,
    retention: Mutex<Retention>,
    dropped: AtomicU64,
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
        let retention = prefs.retention();
        std::thread::Builder::new()
            .name("ninesql-history".into())
            .spawn(move || writer(db, &rx, retention))
            .map_err(|_| HistoryError::Closed)?;
        Ok(Self {
            inner: Arc::new(Inner {
                jobs,
                project: project.into(),
                enabled: AtomicBool::new(prefs.enabled),
                retention: Mutex::new(retention),
                dropped: AtomicU64::new(0),
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
                .retention
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let changed = *current != retention;
            *current = retention;
            changed
        };
        if changed {
            let _ = self.inner.jobs.try_send(Job::Prune(retention, None));
        }
    }

    /// Entries dropped because the writer's queue was full.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.inner.dropped.load(Ordering::Relaxed)
    }

    /// Records a finished statement if [`may_record`] allows it; never blocks.
    pub fn record(&self, policy: &SafetyPolicy, record: StatementRecord) -> Recorded {
        if !self.inner.enabled.load(Ordering::Relaxed) {
            return Recorded::Disabled;
        }
        if !policy.log_query_text || policy.production {
            return Recorded::NotAllowed;
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
                self.inner.dropped.fetch_add(1, Ordering::Relaxed);
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
        let retention = *self
            .inner
            .retention
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        self.ask(|tx| Job::Prune(retention, Some(tx)))
    }

    /// Answers once every entry queued before it is written (and the write-ahead log is
    /// checkpointed into the main file).
    pub fn flush(&self) -> Reply<Result<(), HistoryError>> {
        self.ask(Job::Flush)
    }
}

fn writer(mut db: HistoryDb, jobs: &mpsc::Receiver<Job>, mut retention: Retention) {
    let prune = |db: &mut HistoryDb, retention: &Retention| match db.prune(retention, now_ms()) {
        Ok(n) => {
            if n > 0 {
                tracing::debug!(deleted = n, "history pruned");
            }
            Ok(n)
        }
        Err(e) => {
            tracing::warn!(error = %e, "history pruning failed");
            Err(HistoryError::from(e))
        }
    };
    let _ = prune(&mut db, &retention);
    let mut since_prune = 0u64;
    while let Ok(job) = jobs.recv() {
        match job {
            Job::Insert(entry) => {
                if let Err(e) = db.insert(&entry) {
                    tracing::warn!(error = %e, "history entry not written");
                }
                since_prune += 1;
                if since_prune >= PRUNE_EVERY {
                    since_prune = 0;
                    let _ = prune(&mut db, &retention);
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
            Job::Prune(r, tx) => {
                retention = r;
                since_prune = 0;
                let result = prune(&mut db, &retention);
                if let Some(tx) = tx {
                    let _ = tx.send(result);
                }
            }
            Job::Flush(tx) => {
                let _ = tx.send(db.checkpoint().map_err(HistoryError::from));
            }
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
