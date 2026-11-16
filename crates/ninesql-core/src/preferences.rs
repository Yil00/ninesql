//! Typed execution preferences (F-WS-04), stored in `settings.toml` (ADR-0008 §1: "user
//! preferences … defaults"). ROADMAP: P3-C. No UI here: the preferences page (P3-A / later)
//! reads and changes these through [`crate::CoreServices::preferences`] and
//! [`crate::CoreServices::save_preferences`].
//!
//! # Sections
//! ```toml
//! [execution]
//! query_timeout_secs = 0          # 0 = no timeout (ADR-0004 interactive default)
//! fetch_size = 200                # rows per batch, 1..=10000 (DBeaver's "fetch size")
//! result_memory_mib = 256         # memory budget of one result, 1..=16384
//! max_value_kib = 1024            # longer values are cut and flagged, 1..=1048576
//! on_error = "stop"               # scripts: "stop" or "continue"
//! keep_alive_secs = 0             # 0 = off, else 10..=86400
//! auto_reconnect = true           # the UI offers / starts a reconnect when a session is lost
//! reconnect_attempts = 3          # 1..=20
//! paused_result_idle_secs = 240   # 30..=280: a paused result is closed after this long idle
//!
//! [execution.connection_timeouts] # per-connection default timeout, by connection id
//! "6f1c…" = 30
//!
//! [history]
//! enabled = true
//! max_entries_per_connection = 10000   # 0..=1000000
//! max_age_days = 90                    # 0 = no age limit, else 1..=36500
//!
//! [confirmations]
//! close_with_open_transaction = true   # always asked for production (ADR-0007 §5)
//! ```
//!
//! # Rules
//! - **Lenient load, strict save.** The file is hand-edited: [`Preferences::parse`] keeps the
//!   default of every missing or invalid key and reports each invalid one as a
//!   [`PreferenceIssue`] (never echoing a string value); unknown keys are ignored.
//!   [`PreferenceChange`]s are validated before anything is written
//!   ([`PreferencesError::Invalid`]).
//! - **Nothing weakens ADR-0007.** Whether destructive statements and writes are confirmed is a
//!   property of the connection type, not a preference: there is deliberately no key for it.
//!   `close_with_open_transaction = false` does not apply to production connections.
//! - **Only what changed.** [`apply_changes`] edits the given keys with `toml_edit` and keeps
//!   comments, formatting and every other key and section as they are (the same approach as
//!   the UI's `persist` module). A file with a newer `schema_version`, invalid TOML or UTF-8 is
//!   never overwritten. Writes are atomic and private ([`ninesql_storage::fs::write_atomic`]).
//! - **One writer at a time in the process.** [`save_changes`] holds a process-wide lock; the UI
//!   writer of `settings.toml` (appearance, key bindings) should take
//!   [`settings_write_lock`] too, so the two never interleave a read-modify-write.
//! - **A paused result is closed before the drivers give up on it.** `paused_result_idle_secs`
//!   stays below 300 s, the time after which pgwire (`IDLE_CURSOR_TIMEOUT`, enforced by the
//!   server's `idle_in_transaction_session_timeout`) and mysqlwire (the server's
//!   `net_write_timeout`) end a session whose result is left waiting, which on mysqlwire in
//!   manual commit also rolls back the user's open transaction (phase 3 review M1). The 20 s
//!   margin covers the release reaching the server. A value set directly in memory is still
//!   clamped to [`PAUSED_RESULT_IDLE_SECS_RANGE`] when a run uses it
//!   ([`crate::paused_result_idle_limit`]).
//! - No secret is ever stored here (I1); connection ids are not secrets.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use ninesql_driver_api::FetchBudget;
use ninesql_storage::document::SCHEMA_VERSION_KEY;
use ninesql_storage::fs::{ensure_private_dir, read_limited, write_atomic};
use ninesql_storage::{Retention, StorageError};
use toml_edit::{DocumentMut, Item, Table, Value};
use uuid::Uuid;

use crate::connection::ConnectionId;

/// `schema_version` of `settings.toml` this build reads and writes.
pub const SETTINGS_SCHEMA_VERSION: i64 = 1;

/// Largest `settings.toml` read (it is a small hand-edited file).
const MAX_SETTINGS_BYTES: u64 = 1024 * 1024;

const MIB: u64 = 1024 * 1024;
const KIB: u64 = 1024;

/// Valid range of `fetch_size`.
pub const FETCH_SIZE_RANGE: (u32, u32) = (1, FetchBudget::MAX_BATCH_ROWS);
/// Valid range of `result_memory_mib`.
pub const RESULT_MEMORY_MIB_RANGE: (u64, u64) = (1, 16 * 1024);
/// Valid range of `max_value_kib`.
pub const MAX_VALUE_KIB_RANGE: (u64, u64) = (1, 1024 * 1024);
/// Valid range of a non-zero `query_timeout_secs` (and per-connection timeouts).
pub const QUERY_TIMEOUT_SECS_RANGE: (u64, u64) = (1, 7 * 24 * 60 * 60);
/// Valid range of a non-zero `keep_alive_secs`.
pub const KEEP_ALIVE_SECS_RANGE: (u64, u64) = (10, 24 * 60 * 60);
/// Valid range of `reconnect_attempts`.
pub const RECONNECT_ATTEMPTS_RANGE: (u32, u32) = (1, 20);
/// Valid range of `paused_result_idle_secs`: the upper bound stays below the 300 s after which
/// the drivers end a session whose paused result is left waiting (see the module's rules).
pub const PAUSED_RESULT_IDLE_SECS_RANGE: (u64, u64) = (30, 280);
/// Default of `paused_result_idle_secs`.
pub const DEFAULT_PAUSED_RESULT_IDLE_SECS: u64 = 240;
/// Valid range of `max_entries_per_connection`.
pub const HISTORY_ENTRIES_RANGE: (u32, u32) = (0, 1_000_000);
/// Valid range of a non-zero `max_age_days`.
pub const HISTORY_AGE_DAYS_RANGE: (u32, u32) = (1, 36_500);

/// What a script does after a statement fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum OnError {
    /// Stop at the failed statement (default).
    #[default]
    Stop,
    /// Go on with the next statement.
    Continue,
}

impl OnError {
    /// The value in `settings.toml`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::Continue => "continue",
        }
    }
}

/// `[execution]`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ExecutionPreferences {
    /// Default statement timeout; `None`: none (ADR-0004 interactive default).
    pub default_timeout: Option<Duration>,
    /// Rows per batch.
    pub fetch_size: u32,
    /// Memory budget of one result, in bytes.
    pub result_memory_bytes: u64,
    /// Longest value kept whole, in bytes.
    pub max_value_bytes: u64,
    /// Scripts: stop or go on after an error.
    pub on_error: OnError,
    /// Ping idle sessions this often; `None`: off.
    pub keep_alive: Option<Duration>,
    /// The UI reconnects (or offers to) when a session is lost.
    pub auto_reconnect: bool,
    /// Connect attempts of one reconnect.
    pub reconnect_attempts: u32,
    /// Per-connection default timeouts, overriding [`Self::default_timeout`].
    pub connection_timeouts: BTreeMap<ConnectionId, Duration>,
    /// A paused paged result ([`crate::RunRequest::with_paging`]) with no fetch for longer
    /// than this is closed by its run ([`crate::RunEvent::ResultClosedIdle`]). Saved and
    /// loaded within [`PAUSED_RESULT_IDLE_SECS_RANGE`]; clamped to it when used.
    pub paused_result_idle: Duration,
}

impl Default for ExecutionPreferences {
    fn default() -> Self {
        Self {
            default_timeout: None,
            fetch_size: FetchBudget::DEFAULT_BATCH_ROWS,
            result_memory_bytes: FetchBudget::DEFAULT_MAX_BYTES,
            max_value_bytes: FetchBudget::DEFAULT_MAX_VALUE_BYTES,
            on_error: OnError::Stop,
            keep_alive: None,
            auto_reconnect: true,
            reconnect_attempts: 3,
            connection_timeouts: BTreeMap::new(),
            paused_result_idle: Duration::from_secs(DEFAULT_PAUSED_RESULT_IDLE_SECS),
        }
    }
}

impl ExecutionPreferences {
    /// The fetch budget of a statement under these preferences.
    #[must_use]
    pub fn budget(&self) -> FetchBudget {
        let max_bytes = self.result_memory_bytes.max(1);
        FetchBudget::default()
            .with_batch_rows(self.fetch_size)
            .with_max_bytes(max_bytes)
            .with_max_batch_bytes(FetchBudget::DEFAULT_MAX_BATCH_BYTES.min(max_bytes))
            .with_max_value_bytes(self.max_value_bytes.min(max_bytes))
            .with_max_row_bytes(
                FetchBudget::DEFAULT_MAX_ROW_BYTES
                    .max(self.max_value_bytes)
                    .min(max_bytes),
            )
    }

    /// The default timeout of statements on `connection`: its own, else the global one.
    #[must_use]
    pub fn timeout_for(&self, connection: ConnectionId) -> Option<Duration> {
        self.connection_timeouts
            .get(&connection)
            .copied()
            .or(self.default_timeout)
    }
}

/// `[history]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct HistoryPreferences {
    /// Record statements at all (the connection type rules apply on top, see
    /// [`crate::history`]).
    pub enabled: bool,
    /// Entries kept per connection.
    pub max_entries_per_connection: u32,
    /// Entries older than this many days are deleted; `None`: no age limit.
    pub max_age_days: Option<u32>,
}

impl Default for HistoryPreferences {
    fn default() -> Self {
        Self {
            enabled: true,
            max_entries_per_connection:
                ninesql_storage::history::DEFAULT_MAX_ENTRIES_PER_CONNECTION,
            max_age_days: Some(90),
        }
    }
}

impl HistoryPreferences {
    /// The retention rules for the history database.
    #[must_use]
    pub fn retention(&self) -> Retention {
        Retention {
            max_entries_per_connection: self.max_entries_per_connection,
            max_age: self
                .max_age_days
                .map(|d| Duration::from_secs(u64::from(d) * 24 * 60 * 60)),
        }
    }
}

/// `[confirmations]`. Statement confirmations are set by connection types (ADR-0007), not here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ConfirmationPreferences {
    /// Ask before closing a session (tab, window, application) with an open transaction. Always
    /// asked for production connections, whatever this says (ADR-0007 §5).
    pub close_with_open_transaction: bool,
}

impl Default for ConfirmationPreferences {
    fn default() -> Self {
        Self {
            close_with_open_transaction: true,
        }
    }
}

impl ConfirmationPreferences {
    /// Whether closing a session of a connection (production or not) with an open transaction
    /// asks first.
    #[must_use]
    pub fn asks_on_close(&self, production: bool) -> bool {
        production || self.close_with_open_transaction
    }
}

/// Every execution preference.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct Preferences {
    /// `[execution]`.
    pub execution: ExecutionPreferences,
    /// `[history]`.
    pub history: HistoryPreferences,
    /// `[confirmations]`.
    pub confirmations: ConfirmationPreferences,
}

/// An invalid entry found while reading `settings.toml`. The message names the key and the
/// rule, never a string value from the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreferenceIssue(String);

impl PreferenceIssue {
    /// What is wrong, for the settings problems list.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for PreferenceIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why preferences could not be changed or saved.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PreferencesError {
    /// A value outside its range.
    #[error("`{key}`: {reason}")]
    Invalid {
        /// Key, as `section.key`.
        key: &'static str,
        /// Rule broken.
        reason: String,
    },
    /// `settings.toml` is not valid TOML / UTF-8 any more, or a section is not a table: it is
    /// never overwritten.
    #[error("settings.toml cannot be updated: {0}")]
    Unreadable(&'static str),
    /// `settings.toml` was written by a newer NineSQL: it is never overwritten.
    #[error("settings.toml has schema_version {0}, newer than this NineSQL writes")]
    NewerVersion(i64),
    /// File system error.
    #[error(transparent)]
    Storage(#[from] StorageError),
}

/// One preference changed (by the preferences page).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PreferenceChange {
    /// `execution.query_timeout_secs`; `None` = no timeout.
    DefaultTimeout(Option<Duration>),
    /// `execution.fetch_size`.
    FetchSize(u32),
    /// `execution.result_memory_mib` (the value is in bytes, a whole number of MiB).
    ResultMemoryBytes(u64),
    /// `execution.max_value_kib` (the value is in bytes, a whole number of KiB).
    MaxValueBytes(u64),
    /// `execution.on_error`.
    OnError(OnError),
    /// `execution.keep_alive_secs`; `None` = off.
    KeepAlive(Option<Duration>),
    /// `execution.auto_reconnect`.
    AutoReconnect(bool),
    /// `execution.reconnect_attempts`.
    ReconnectAttempts(u32),
    /// `execution.connection_timeouts."<id>"`; `None` removes the override.
    ConnectionTimeout(ConnectionId, Option<Duration>),
    /// `execution.paused_result_idle_secs` (whole seconds, within
    /// [`PAUSED_RESULT_IDLE_SECS_RANGE`]).
    PausedResultIdle(Duration),
    /// `history.enabled`.
    HistoryEnabled(bool),
    /// `history.max_entries_per_connection`.
    HistoryMaxEntries(u32),
    /// `history.max_age_days`; `None` = no age limit.
    HistoryMaxAgeDays(Option<u32>),
    /// `confirmations.close_with_open_transaction`.
    CloseWithOpenTransaction(bool),
}

fn out_of_range<T: std::fmt::Display>(key: &'static str, lo: T, hi: T) -> PreferencesError {
    PreferencesError::Invalid {
        key,
        reason: format!("must be between {lo} and {hi}"),
    }
}

fn check_range<T: PartialOrd + std::fmt::Display + Copy>(
    key: &'static str,
    v: T,
    (lo, hi): (T, T),
) -> Result<T, PreferencesError> {
    if v < lo || v > hi {
        Err(out_of_range(key, lo, hi))
    } else {
        Ok(v)
    }
}

fn whole_seconds(key: &'static str, d: Duration) -> Result<u64, PreferencesError> {
    if d.subsec_nanos() != 0 {
        return Err(PreferencesError::Invalid {
            key,
            reason: "must be a whole number of seconds".into(),
        });
    }
    Ok(d.as_secs())
}

impl PreferenceChange {
    /// The `section.key` it writes.
    #[must_use]
    pub fn key(&self) -> &'static str {
        match self {
            Self::DefaultTimeout(_) => "execution.query_timeout_secs",
            Self::FetchSize(_) => "execution.fetch_size",
            Self::ResultMemoryBytes(_) => "execution.result_memory_mib",
            Self::MaxValueBytes(_) => "execution.max_value_kib",
            Self::OnError(_) => "execution.on_error",
            Self::KeepAlive(_) => "execution.keep_alive_secs",
            Self::AutoReconnect(_) => "execution.auto_reconnect",
            Self::ReconnectAttempts(_) => "execution.reconnect_attempts",
            Self::ConnectionTimeout(..) => "execution.connection_timeouts",
            Self::PausedResultIdle(_) => "execution.paused_result_idle_secs",
            Self::HistoryEnabled(_) => "history.enabled",
            Self::HistoryMaxEntries(_) => "history.max_entries_per_connection",
            Self::HistoryMaxAgeDays(_) => "history.max_age_days",
            Self::CloseWithOpenTransaction(_) => "confirmations.close_with_open_transaction",
        }
    }

    /// Checks the value and applies it to `prefs`.
    ///
    /// # Errors
    /// [`PreferencesError::Invalid`]; `prefs` is unchanged then.
    pub fn apply(&self, prefs: &mut Preferences) -> Result<(), PreferencesError> {
        let key = self.key();
        let e = &mut prefs.execution;
        match *self {
            Self::DefaultTimeout(t) => {
                if let Some(d) = t {
                    check_range(key, whole_seconds(key, d)?, QUERY_TIMEOUT_SECS_RANGE)?;
                }
                e.default_timeout = t;
            }
            Self::FetchSize(n) => e.fetch_size = check_range(key, n, FETCH_SIZE_RANGE)?,
            Self::ResultMemoryBytes(b) => {
                if b % MIB != 0 {
                    return Err(PreferencesError::Invalid {
                        key,
                        reason: "must be a whole number of MiB".into(),
                    });
                }
                check_range(key, b / MIB, RESULT_MEMORY_MIB_RANGE)?;
                e.result_memory_bytes = b;
            }
            Self::MaxValueBytes(b) => {
                if b % KIB != 0 {
                    return Err(PreferencesError::Invalid {
                        key,
                        reason: "must be a whole number of KiB".into(),
                    });
                }
                check_range(key, b / KIB, MAX_VALUE_KIB_RANGE)?;
                e.max_value_bytes = b;
            }
            Self::OnError(o) => e.on_error = o,
            Self::KeepAlive(k) => {
                if let Some(d) = k {
                    check_range(key, whole_seconds(key, d)?, KEEP_ALIVE_SECS_RANGE)?;
                }
                e.keep_alive = k;
            }
            Self::AutoReconnect(on) => e.auto_reconnect = on,
            Self::ReconnectAttempts(n) => {
                e.reconnect_attempts = check_range(key, n, RECONNECT_ATTEMPTS_RANGE)?;
            }
            Self::ConnectionTimeout(id, t) => match t {
                Some(d) => {
                    check_range(key, whole_seconds(key, d)?, QUERY_TIMEOUT_SECS_RANGE)?;
                    e.connection_timeouts.insert(id, d);
                }
                None => {
                    e.connection_timeouts.remove(&id);
                }
            },
            Self::PausedResultIdle(d) => {
                check_range(key, whole_seconds(key, d)?, PAUSED_RESULT_IDLE_SECS_RANGE)?;
                e.paused_result_idle = d;
            }
            Self::HistoryEnabled(on) => prefs.history.enabled = on,
            Self::HistoryMaxEntries(n) => {
                prefs.history.max_entries_per_connection =
                    check_range(key, n, HISTORY_ENTRIES_RANGE)?;
            }
            Self::HistoryMaxAgeDays(d) => {
                if let Some(d) = d {
                    check_range(key, d, HISTORY_AGE_DAYS_RANGE)?;
                }
                prefs.history.max_age_days = d;
            }
            Self::CloseWithOpenTransaction(on) => {
                prefs.confirmations.close_with_open_transaction = on;
            }
        }
        Ok(())
    }

    /// `(section, key, value)` to write; `None` value removes the key.
    fn toml(&self) -> (&'static str, String, Option<Value>) {
        let int = |v: u64| Some(Value::from(i64::try_from(v).unwrap_or(i64::MAX)));
        let (section, key) = self
            .key()
            .split_once('.')
            .unwrap_or(("execution", self.key()));
        match self {
            Self::DefaultTimeout(t) => (section, key.into(), int(t.map_or(0, |d| d.as_secs()))),
            Self::FetchSize(n) | Self::ReconnectAttempts(n) | Self::HistoryMaxEntries(n) => {
                (section, key.into(), int(u64::from(*n)))
            }
            Self::ResultMemoryBytes(b) => (section, key.into(), int(b / MIB)),
            Self::MaxValueBytes(b) => (section, key.into(), int(b / KIB)),
            Self::OnError(o) => (section, key.into(), Some(Value::from(o.as_str()))),
            Self::KeepAlive(k) => (section, key.into(), int(k.map_or(0, |d| d.as_secs()))),
            Self::PausedResultIdle(d) => (section, key.into(), int(d.as_secs())),
            Self::AutoReconnect(b)
            | Self::HistoryEnabled(b)
            | Self::CloseWithOpenTransaction(b) => (section, key.into(), Some(Value::from(*b))),
            Self::ConnectionTimeout(id, t) => (
                "execution.connection_timeouts",
                id.to_string(),
                t.and_then(|d| int(d.as_secs())),
            ),
            Self::HistoryMaxAgeDays(d) => (section, key.into(), int(u64::from(d.unwrap_or(0)))),
        }
    }
}

/// Reads an integer key of a section, with its range; reports anything else.
struct Reader<'a> {
    section: &'static str,
    table: &'a toml::Table,
    issues: &'a mut Vec<PreferenceIssue>,
}

impl Reader<'_> {
    fn issue(&mut self, key: &str, rule: &str) {
        self.issues.push(PreferenceIssue(format!(
            "[{}] {key}: {rule}; the default is used",
            self.section
        )));
    }

    fn int<T>(&mut self, key: &str, (lo, hi): (T, T), zero_allowed: bool) -> Option<T>
    where
        T: TryFrom<i64> + Into<i128> + Copy + std::fmt::Display,
    {
        let value = self.table.get(key)?;
        let Some(v) = value.as_integer() else {
            self.issue(key, "an integer is expected");
            return None;
        };
        if zero_allowed && v == 0 {
            return T::try_from(0).ok();
        }
        let in_range = i128::from(v) >= lo.into() && i128::from(v) <= hi.into();
        match T::try_from(v) {
            Ok(t) if in_range => Some(t),
            _ => {
                let zero = if zero_allowed { "0 or " } else { "" };
                self.issue(key, &format!("must be {zero}between {lo} and {hi}"));
                None
            }
        }
    }

    fn bool(&mut self, key: &str) -> Option<bool> {
        let value = self.table.get(key)?;
        let b = value.as_bool();
        if b.is_none() {
            self.issue(key, "true or false is expected");
        }
        b
    }
}

impl Preferences {
    /// Reads the preferences from the text of `settings.toml`, leniently: every missing or
    /// invalid key keeps its default, and each invalid one is reported. A file with a newer
    /// `schema_version` is not read at all (defaults, one issue). Never fails.
    #[must_use]
    pub fn parse(text: &str) -> (Self, Vec<PreferenceIssue>) {
        let mut prefs = Self::default();
        let mut issues = Vec::new();
        let root: toml::Table = match text.parse() {
            Ok(t) => t,
            Err(_) => {
                issues.push(PreferenceIssue(
                    "settings.toml is not valid TOML; default preferences are used".into(),
                ));
                return (prefs, issues);
            }
        };
        if let Some(v) = root
            .get(SCHEMA_VERSION_KEY)
            .and_then(toml::Value::as_integer)
            && v > SETTINGS_SCHEMA_VERSION
        {
            issues.push(PreferenceIssue(format!(
                "settings.toml has schema_version {v}, newer than this NineSQL reads; default preferences are used"
            )));
            return (prefs, issues);
        }
        let section = |name: &'static str, issues: &mut Vec<PreferenceIssue>| match root.get(name) {
            None => None,
            Some(toml::Value::Table(t)) => Some(t.clone()),
            Some(_) => {
                issues.push(PreferenceIssue(format!(
                    "[{name}] is not a table; its defaults are used"
                )));
                None
            }
        };

        if let Some(table) = section("execution", &mut issues) {
            let mut r = Reader {
                section: "execution",
                table: &table,
                issues: &mut issues,
            };
            let e = &mut prefs.execution;
            if let Some(s) = r.int("query_timeout_secs", QUERY_TIMEOUT_SECS_RANGE, true) {
                e.default_timeout = (s > 0).then(|| Duration::from_secs(s));
            }
            if let Some(n) = r.int("fetch_size", FETCH_SIZE_RANGE, false) {
                e.fetch_size = n;
            }
            if let Some(m) = r.int("result_memory_mib", RESULT_MEMORY_MIB_RANGE, false) {
                e.result_memory_bytes = m * MIB;
            }
            if let Some(k) = r.int("max_value_kib", MAX_VALUE_KIB_RANGE, false) {
                e.max_value_bytes = k * KIB;
            }
            if let Some(v) = table.get("on_error") {
                match v.as_str() {
                    Some("stop") => e.on_error = OnError::Stop,
                    Some("continue") => e.on_error = OnError::Continue,
                    _ => r.issue("on_error", "\"stop\" or \"continue\" is expected"),
                }
            }
            if let Some(s) = r.int("keep_alive_secs", KEEP_ALIVE_SECS_RANGE, true) {
                e.keep_alive = (s > 0).then(|| Duration::from_secs(s));
            }
            if let Some(b) = r.bool("auto_reconnect") {
                e.auto_reconnect = b;
            }
            if let Some(n) = r.int("reconnect_attempts", RECONNECT_ATTEMPTS_RANGE, false) {
                e.reconnect_attempts = n;
            }
            if let Some(s) = r.int(
                "paused_result_idle_secs",
                PAUSED_RESULT_IDLE_SECS_RANGE,
                false,
            ) {
                e.paused_result_idle = Duration::from_secs(s);
            }
            match table.get("connection_timeouts") {
                None => {}
                Some(toml::Value::Table(t)) => {
                    for (id, v) in t {
                        let Ok(uuid) = Uuid::parse_str(id) else {
                            r.issue("connection_timeouts", "a key is not a connection id");
                            continue;
                        };
                        let (lo, hi) = QUERY_TIMEOUT_SECS_RANGE;
                        match v.as_integer().and_then(|s| u64::try_from(s).ok()) {
                            Some(s) if (lo..=hi).contains(&s) => {
                                e.connection_timeouts
                                    .insert(ConnectionId::from_uuid(uuid), Duration::from_secs(s));
                            }
                            _ => r.issue(
                                "connection_timeouts",
                                &format!("a timeout must be between {lo} and {hi} seconds"),
                            ),
                        }
                    }
                }
                Some(_) => r.issue("connection_timeouts", "a table is expected"),
            }
        }
        if let Some(table) = section("history", &mut issues) {
            let mut r = Reader {
                section: "history",
                table: &table,
                issues: &mut issues,
            };
            if let Some(b) = r.bool("enabled") {
                prefs.history.enabled = b;
            }
            if let Some(n) = r.int("max_entries_per_connection", HISTORY_ENTRIES_RANGE, true) {
                prefs.history.max_entries_per_connection = n;
            }
            if let Some(d) = r.int("max_age_days", HISTORY_AGE_DAYS_RANGE, true) {
                prefs.history.max_age_days = (d > 0).then_some(d);
            }
        }
        if let Some(table) = section("confirmations", &mut issues) {
            let mut r = Reader {
                section: "confirmations",
                table: &table,
                issues: &mut issues,
            };
            if let Some(b) = r.bool("close_with_open_transaction") {
                prefs.confirmations.close_with_open_transaction = b;
            }
        }
        (prefs, issues)
    }

    /// Reads `path` (usually [`ninesql_storage::Paths::settings_file`]); a missing file gives
    /// the defaults. **Blocking.** The last path component is never followed if it is a
    /// symbolic link: a caller that supports a linked `settings.toml` passes the resolved
    /// target.
    ///
    /// # Errors
    /// [`PreferencesError::Storage`] (I/O, not a regular file, too large),
    /// [`PreferencesError::Unreadable`] when the file is not UTF-8.
    pub fn load(path: &Path) -> Result<(Self, Vec<PreferenceIssue>), PreferencesError> {
        match read_limited(path, MAX_SETTINGS_BYTES)? {
            None => Ok((Self::default(), Vec::new())),
            Some(bytes) => {
                let text = String::from_utf8(bytes)
                    .map_err(|_| PreferencesError::Unreadable("it is not valid UTF-8"))?;
                Ok(Self::parse(&text))
            }
        }
    }
}

/// The process-wide lock of `settings.toml` read-modify-write cycles. [`save_changes`] holds
/// it; any other writer of the file in the process should hold it too.
pub fn settings_write_lock() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Applies `changes` to the text of `settings.toml` and returns the new text: only the changed
/// keys are edited, everything else (comments, formatting, other sections, unknown keys) is
/// kept. `schema_version` is kept when present and added when missing. Pure.
///
/// # Errors
/// [`PreferencesError::Invalid`] for a value out of range (checked against the current
/// preferences of `text`), [`PreferencesError::Unreadable`] for invalid TOML or a section that
/// is not a table, [`PreferencesError::NewerVersion`].
pub fn apply_changes(text: &str, changes: &[PreferenceChange]) -> Result<String, PreferencesError> {
    let mut check = Preferences::parse(text).0;
    for c in changes {
        c.apply(&mut check)?;
    }
    let mut doc: DocumentMut = text
        .parse()
        .map_err(|_| PreferencesError::Unreadable("it is not valid TOML"))?;
    match doc.get(SCHEMA_VERSION_KEY) {
        None => {
            doc.insert(
                SCHEMA_VERSION_KEY,
                toml_edit::value(SETTINGS_SCHEMA_VERSION),
            );
        }
        Some(item) => match item.as_integer() {
            Some(v) if v > SETTINGS_SCHEMA_VERSION => {
                return Err(PreferencesError::NewerVersion(v));
            }
            Some(_) => {}
            None => {
                return Err(PreferencesError::Unreadable(
                    "its schema_version is not an integer",
                ));
            }
        },
    }
    for change in changes {
        let (section, key, value) = change.toml();
        let table = section_table(&mut doc, section)?;
        match value {
            Some(mut v) => match table.get_mut(&key).and_then(Item::as_value_mut) {
                // Replaced in place: the key, its comments and the value's trailing comment stay.
                Some(current) => {
                    *v.decor_mut() = current.decor().clone();
                    *current = v;
                }
                None => {
                    table.insert(&key, Item::Value(v));
                }
            },
            None => {
                table.remove(&key);
            }
        }
    }
    Ok(doc.to_string())
}

/// The table at a dotted `section` path, created (non-inline) when missing.
fn section_table<'a>(
    doc: &'a mut DocumentMut,
    section: &str,
) -> Result<&'a mut Table, PreferencesError> {
    let mut table = doc.as_table_mut();
    for part in section.split('.') {
        let entry = table.entry(part).or_insert_with(|| {
            let mut t = Table::new();
            t.set_implicit(false);
            Item::Table(t)
        });
        table = entry.as_table_mut().ok_or(PreferencesError::Unreadable(
            "a preferences section is not a table",
        ))?;
    }
    Ok(table)
}

/// Writes `changes` to `path` (usually [`ninesql_storage::Paths::settings_file`]) and returns
/// the preferences now in the file. **Blocking.** Holds [`settings_write_lock`]; creates a
/// missing configuration directory `0700`; writes atomically, `0600`. The last path component
/// is never followed if it is a symbolic link (pass the resolved target, as for
/// [`Preferences::load`]).
///
/// # Errors
/// As [`apply_changes`], and [`PreferencesError::Storage`].
pub fn save_changes(
    path: &Path,
    changes: &[PreferenceChange],
) -> Result<Preferences, PreferencesError> {
    let _guard = settings_write_lock();
    if let Some(dir) = path.parent().filter(|p| !p.as_os_str().is_empty())
        && std::fs::symlink_metadata(dir).is_err()
    {
        ensure_private_dir(dir)?;
    }
    let text = match read_limited(path, MAX_SETTINGS_BYTES)? {
        None => String::new(),
        Some(bytes) => String::from_utf8(bytes)
            .map_err(|_| PreferencesError::Unreadable("it is not valid UTF-8"))?,
    };
    let new_text = apply_changes(&text, changes)?;
    write_atomic(path, new_text.as_bytes())?;
    tracing::debug!(
        changes = changes.len(),
        "preferences saved to settings.toml"
    );
    Ok(Preferences::parse(&new_text).0)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).
mod tests {
    use super::*;

    #[test]
    fn defaults_follow_adr_0004_and_0008() {
        let p = Preferences::default();
        assert_eq!(p.execution.default_timeout, None, "ADR-0004: no timeout");
        assert_eq!(p.execution.fetch_size, 200);
        assert_eq!(p.execution.budget(), FetchBudget::default());
        assert_eq!(p.execution.paused_result_idle, Duration::from_secs(240));
        assert_eq!(p.history.retention(), Retention::default(), "ADR-0008 §6");
        assert!(p.confirmations.asks_on_close(false));
        let (parsed, issues) = Preferences::parse("");
        assert_eq!(parsed, p);
        assert!(issues.is_empty());
    }

    #[test]
    fn parses_valid_values_and_reports_invalid_ones() {
        let id = ConnectionId::new_random();
        let text = format!(
            r#"
schema_version = 1
[appearance]
theme = "dark"
[execution]
query_timeout_secs = 30
fetch_size = 500
result_memory_mib = 64
max_value_kib = 512
on_error = "continue"
keep_alive_secs = 120
auto_reconnect = false
reconnect_attempts = 5
paused_result_idle_secs = 60
unknown = 1
[execution.connection_timeouts]
"{id}" = 5
"not-a-uuid" = 3
[history]
enabled = false
max_entries_per_connection = 0
max_age_days = 0
[confirmations]
close_with_open_transaction = false
"#
        );
        let (p, issues) = Preferences::parse(&text);
        assert_eq!(issues.len(), 1, "{issues:?}");
        let e = &p.execution;
        assert_eq!(e.default_timeout, Some(Duration::from_secs(30)));
        assert_eq!(e.fetch_size, 500);
        assert_eq!(e.result_memory_bytes, 64 * MIB);
        assert_eq!(e.max_value_bytes, 512 * KIB);
        assert_eq!(e.on_error, OnError::Continue);
        assert_eq!(e.keep_alive, Some(Duration::from_secs(120)));
        assert!(!e.auto_reconnect);
        assert_eq!(e.reconnect_attempts, 5);
        assert_eq!(e.paused_result_idle, Duration::from_secs(60));
        assert_eq!(e.timeout_for(id), Some(Duration::from_secs(5)));
        assert_eq!(
            e.timeout_for(ConnectionId::new_random()),
            Some(Duration::from_secs(30))
        );
        assert_eq!(e.budget().max_bytes, 64 * MIB);
        assert_eq!(e.budget().batch_rows, 500);
        assert!(!p.history.enabled);
        assert_eq!(p.history.max_entries_per_connection, 0);
        assert_eq!(p.history.max_age_days, None);
        assert!(!p.confirmations.close_with_open_transaction);
        assert!(
            p.confirmations.asks_on_close(true),
            "production always asks"
        );

        let (p, issues) = Preferences::parse(
            "[execution]\nfetch_size = 0\nkeep_alive_secs = 3\non_error = 'maybe'\nauto_reconnect = 'yes'\nresult_memory_mib = 'big secret'\npaused_result_idle_secs = 300\n[history]\nmax_age_days = -1",
        );
        assert_eq!(p, Preferences::default());
        assert_eq!(issues.len(), 7, "{issues:?}");
        // At or above the drivers' 300 s, or 0, is refused: the default stays.
        for bad in ["0", "29", "281", "300", "-1"] {
            let (p, issues) =
                Preferences::parse(&format!("[execution]\npaused_result_idle_secs = {bad}"));
            assert_eq!(p, Preferences::default(), "{bad}");
            assert_eq!(issues.len(), 1, "{bad}");
        }
        assert!(
            issues.iter().all(|i| !i.message().contains("big secret")),
            "string values are never echoed"
        );
        let (p, issues) = Preferences::parse("schema_version = 2\n[execution]\nfetch_size = 9");
        assert_eq!(p, Preferences::default());
        assert_eq!(issues.len(), 1);
        assert_eq!(Preferences::parse("not toml [").1.len(), 1);
        assert_eq!(Preferences::parse("execution = 3").1.len(), 1);
    }

    #[test]
    fn changes_are_validated() {
        let mut p = Preferences::default();
        for bad in [
            PreferenceChange::FetchSize(0),
            PreferenceChange::FetchSize(FetchBudget::MAX_BATCH_ROWS + 1),
            PreferenceChange::ResultMemoryBytes(MIB + 1),
            PreferenceChange::ResultMemoryBytes(0),
            PreferenceChange::MaxValueBytes(100),
            PreferenceChange::DefaultTimeout(Some(Duration::from_millis(1500))),
            PreferenceChange::KeepAlive(Some(Duration::from_secs(1))),
            PreferenceChange::ReconnectAttempts(0),
            PreferenceChange::HistoryMaxAgeDays(Some(0)),
            PreferenceChange::ConnectionTimeout(ConnectionId::new_random(), Some(Duration::ZERO)),
            PreferenceChange::PausedResultIdle(Duration::from_secs(29)),
            PreferenceChange::PausedResultIdle(Duration::from_secs(281)),
            PreferenceChange::PausedResultIdle(Duration::from_secs(300)),
            PreferenceChange::PausedResultIdle(Duration::from_millis(60_500)),
        ] {
            assert!(
                matches!(bad.apply(&mut p), Err(PreferencesError::Invalid { .. })),
                "{bad:?}"
            );
        }
        assert_eq!(p, Preferences::default(), "unchanged on error");
    }

    #[test]
    fn apply_changes_keeps_everything_else() {
        let id = ConnectionId::new_random();
        let text = "# my settings\nschema_version = 1\n\n[appearance]\ntheme = \"dark\" # keep me\n\n[execution]\n# comment kept\nfetch_size = 100\n";
        let out = apply_changes(
            text,
            &[
                PreferenceChange::FetchSize(1000),
                PreferenceChange::DefaultTimeout(Some(Duration::from_secs(60))),
                PreferenceChange::ConnectionTimeout(id, Some(Duration::from_secs(9))),
                PreferenceChange::HistoryMaxAgeDays(None),
                PreferenceChange::CloseWithOpenTransaction(false),
                PreferenceChange::PausedResultIdle(Duration::from_secs(120)),
            ],
        )
        .unwrap();
        assert!(out.contains("# my settings"));
        assert!(out.contains("theme = \"dark\" # keep me"));
        assert!(out.contains("# comment kept"));
        let (p, issues) = Preferences::parse(&out);
        assert!(issues.is_empty(), "{issues:?}\n{out}");
        assert_eq!(p.execution.fetch_size, 1000);
        assert_eq!(p.execution.default_timeout, Some(Duration::from_secs(60)));
        assert_eq!(p.execution.timeout_for(id), Some(Duration::from_secs(9)));
        assert_eq!(p.history.max_age_days, None);
        assert!(!p.confirmations.close_with_open_transaction);
        assert_eq!(p.execution.paused_result_idle, Duration::from_secs(120));
        assert!(out.contains("paused_result_idle_secs = 120"));
        let out = apply_changes(&out, &[PreferenceChange::ConnectionTimeout(id, None)]).unwrap();
        assert_eq!(
            Preferences::parse(&out).0.execution.timeout_for(id),
            Some(Duration::from_secs(60))
        );

        let fresh = apply_changes("", &[PreferenceChange::OnError(OnError::Continue)]).unwrap();
        assert!(fresh.contains("schema_version = 1"));
        assert_eq!(
            Preferences::parse(&fresh).0.execution.on_error,
            OnError::Continue
        );

        assert!(matches!(
            apply_changes("schema_version = 3", &[PreferenceChange::FetchSize(10)]),
            Err(PreferencesError::NewerVersion(3))
        ));
        assert!(matches!(
            apply_changes("x = [", &[PreferenceChange::FetchSize(10)]),
            Err(PreferencesError::Unreadable(_))
        ));
        assert!(matches!(
            apply_changes("execution = 1", &[PreferenceChange::FetchSize(10)]),
            Err(PreferencesError::Unreadable(_))
        ));
        assert!(matches!(
            apply_changes("", &[PreferenceChange::FetchSize(0)]),
            Err(PreferencesError::Invalid { .. })
        ));
    }

    #[test]
    fn save_and_load_round_trip_privately() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config/ninesql/settings.toml");
        assert_eq!(Preferences::load(&path).unwrap().0, Preferences::default());
        let p = save_changes(&path, &[PreferenceChange::HistoryMaxEntries(50)]).unwrap();
        assert_eq!(p.history.max_entries_per_connection, 50);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let (loaded, issues) = Preferences::load(&path).unwrap();
        assert!(issues.is_empty());
        assert_eq!(loaded, p);
        std::fs::write(&path, "schema_version = 2\n").unwrap();
        assert!(matches!(
            save_changes(&path, &[PreferenceChange::FetchSize(10)]),
            Err(PreferencesError::NewerVersion(2))
        ));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "schema_version = 2\n"
        );
    }
}
