//! logind listener for [`crate::lock_events`] (feature `desktop-lock`, on by default and implied
//! by `secret-service`; zbus 6 only, the same zbus as the Secret Service backend).
//!
//! [`DesktopLockWatcher::start`] opens its own system-bus connection on a thread of its own and
//! calls a [`LockHook`] on:
//! - `org.freedesktop.login1.Manager.PrepareForSleep(true)` (suspend or hibernate about to
//!   start). The watcher holds a logind **delay inhibitor** for sleep (`Inhibit("sleep", …,
//!   "delay")`), so logind waits (up to its `InhibitDelayMaxSec`) until the hook has run; the
//!   inhibitor is released right after the hook, and taken again on `PrepareForSleep(false)`
//!   and whenever logind (re)appears on the bus with a new owner;
//! - the screen lock of the session this process belongs to: the session's `Lock` signal, and
//!   its `LockedHint` property turning `true` (`PropertiesChanged`; GNOME and KDE lockers set the
//!   hint rather than emit `Lock`). The session is found with `GetSessionByPID`, else
//!   `$XDG_SESSION_ID`, else the user's display session (`GetUser(uid)` → `User.Display`, for
//!   applications started in an `app-*.scope` outside the session). Without a session, only sleep
//!   is watched ([`DesktopLockWatcher::watches_session_lock`]).
//!
//! Every subscription is restricted to signals sent by the owner of `org.freedesktop.login1`.
//! Since a signal sent to this connection by name bypasses match rules, each signal is also
//! checked on receipt: no destination header, sender equal to the logind name's current unique
//! owner (`GetNameOwner` at setup, followed through `NameOwnerChanged`), then member, interface
//! and path.
//! On the bus the watcher only calls logind (the session lookup, `Inhibit`, and the session's
//! `LockedHint` after an outage) and the bus itself (`GetNameOwner` for the logind name).
//!
//! Fail closed: a panicking hook is logged and the watcher goes on. When logind leaves the bus,
//! the hook is called once with [`LockTrigger::Requested`] and the watcher waits for a new
//! owner ([`WatcherState::LogindAbsent`]). When the bus connection itself is lost, the hook is
//! called once with [`LockTrigger::Requested`] and the watcher reconnects with a bounded
//! exponential back-off ([`WatcherState::Reconnecting`]: [`RECONNECT_FIRST`] doubling to
//! [`RECONNECT_MAX`]), subscribing again to every signal above. Back on the bus (or when logind
//! returns), the session is looked up again if needed (the session found before is kept
//! meanwhile), a session whose `LockedHint` is `true` or cannot be read locks at once, and a
//! suspend that happened while the bus or logind was gone (`CLOCK_BOOTTIME` ahead of
//! `CLOCK_MONOTONIC` by more than [`MISSED_SLEEP_SLACK`] beyond what it was at the loss) locks
//! too, since its `PrepareForSleep` was never seen. Every wait runs on one timer thread per
//! watcher. [`DesktopLockWatcher::state`]
//! and [`DesktopLockWatcher::is_running`] report all this live. Dropping the watcher stops its
//! thread, during setup, a back-off wait or a reconnect attempt too.

use std::collections::HashMap;
use std::future::{Future, poll_fn};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::{Pin, pin};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError, mpsc};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use zbus::export::futures_core::Stream;
use zbus::message::Type as MessageType;
use zbus::{
    Connection, DynamicType, MatchRule, Message, MessageStream, OwnedFd, OwnedObjectPath,
    OwnedVariant,
};

use crate::lock_events::{LockHook, LockTrigger};

const LOGIND: &str = "org.freedesktop.login1";
const LOGIND_PATH: &str = "/org/freedesktop/login1";
const MANAGER: &str = "org.freedesktop.login1.Manager";
const SESSION: &str = "org.freedesktop.login1.Session";
const USER: &str = "org.freedesktop.login1.User";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";

/// Longest wait for the bus connection, the session lookup and the subscriptions, and timeout
/// of each method call the watcher makes.
pub const WATCHER_SETUP_TIMEOUT: Duration = Duration::from_secs(5);

/// The watcher could not start. No variant carries a secret.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LockWatchError {
    /// No system bus, or a D-Bus failure while subscribing.
    #[error("cannot watch the desktop sleep and lock signals: {0}")]
    Bus(String),
    /// The setup did not finish within [`WATCHER_SETUP_TIMEOUT`].
    #[error("the system bus did not answer in time")]
    Timeout,
}

impl LockWatchError {
    /// A short, stable name of the error kind, safe to log at any level: no D-Bus message
    /// (ADR-0029 §3.2). The message itself is for the UI.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Bus(_) => "lock-watch-bus",
            Self::Timeout => "lock-watch-timeout",
        }
    }
}

/// What failed in a D-Bus call, safe to log at any level (ADR-0029 §3.2): the D-Bus error name
/// of a method error (a dotted name such as `org.freedesktop.DBus.Error.AccessDenied`, whose
/// syntax zbus validates), the I/O error kind of a transport failure, or a fixed label; never
/// the error's free-text description.
fn bus_error_kind(e: &zbus::Error) -> String {
    match e {
        zbus::Error::MethodError(name, _, _) => name.to_string(),
        zbus::Error::FDO(fdo) => zbus::DBusError::name(&**fdo).to_string(),
        zbus::Error::InputOutput(io) | zbus::Error::Connection(io, _) => {
            format!("io-{:?}", io.kind())
        }
        zbus::Error::Handshake(_) => "handshake".to_owned(),
        zbus::Error::Address(_) => "address".to_owned(),
        zbus::Error::InvalidReply => "invalid-reply".to_owned(),
        _ => "dbus-other".to_owned(),
    }
}

/// Which session's lock to watch.
#[derive(Debug, Clone)]
enum SessionChoice {
    /// Ask logind for the session of this process.
    Lookup,
    /// This object path (tests).
    #[cfg(test)]
    Path(String),
}

#[derive(Default)]
struct Stop {
    stopped: AtomicBool,
    waker: Mutex<Option<Waker>>,
}

impl Stop {
    fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        if let Some(w) = self
            .waker
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            w.wake();
        }
    }

    /// Whether the watcher was stopped; otherwise registers the task to be woken when it is.
    fn poll_stopped(&self, cx: &Context<'_>) -> bool {
        if self.stopped.load(Ordering::SeqCst) {
            return true;
        }
        *self.waker.lock().unwrap_or_else(PoisonError::into_inner) = Some(cx.waker().clone());
        // Checked again: a stop between the first check and storing the waker is not lost.
        self.stopped.load(Ordering::SeqCst)
    }
}

/// First wait before reconnecting to a lost system bus. A bus daemon that restarts (rare: a
/// `dbus-broker` or `dbus-daemon` upgrade) is usually back within a second; waiting that long
/// first avoids hammering a socket that is being re-created.
pub const RECONNECT_FIRST: Duration = Duration::from_secs(1);
/// Longest wait between two reconnect attempts (the back-off doubles up to this bound: 1, 2, 4,
/// 8, 16, 32, 60, 60… s). It bounds the time during which a returned bus goes unnoticed to a
/// minute, at the cost of one local connection attempt a minute while the bus stays away.
pub const RECONNECT_MAX: Duration = Duration::from_secs(60);
/// A connection that stayed up this long resets the back-off to [`RECONNECT_FIRST`]; one that
/// is lost sooner keeps doubling it, so a flapping bus is not retried every second.
const RECONNECT_STABLE: Duration = Duration::from_secs(60);
/// More suspended time than this while the bus or logind was away counts as a missed sleep (the
/// two clock reads are not atomic). A suspend shorter than this is not seen.
const MISSED_SLEEP_SLACK: Duration = Duration::from_secs(2);

/// What the watcher does now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum WatcherState {
    /// Listening to logind: sleep (and the session's screen lock when one was found) locks the
    /// secrets.
    Watching,
    /// Connected to the system bus, but `org.freedesktop.login1` has no owner: nothing is
    /// watched until logind (re)appears. When it left while watched, the secrets were locked
    /// then.
    LogindAbsent,
    /// The system-bus connection was lost: the secrets were locked, and the watcher reconnects
    /// with a bounded back-off.
    Reconnecting,
    /// The watcher thread ended (dropped, or it could not run).
    Stopped,
}

impl WatcherState {
    const fn to_u8(self) -> u8 {
        match self {
            Self::Stopped => 0,
            Self::Watching => 1,
            Self::LogindAbsent => 2,
            Self::Reconnecting => 3,
        }
    }

    const fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Watching,
            2 => Self::LogindAbsent,
            3 => Self::Reconnecting,
            _ => Self::Stopped,
        }
    }
}

/// State shared with the watcher thread.
#[derive(Default)]
struct Status {
    state: AtomicU8,
    sleep_delay: AtomicBool,
    session_watched: AtomicBool,
    /// A session's screen lock was watched at some point (sticky).
    had_session: AtomicBool,
    /// logind is back but the session could not be looked up again: the path watched is the
    /// one found on an earlier connection and may not exist any more (re-review N1).
    session_unconfirmed: AtomicBool,
    /// The current connection's unique bus name (tests send unicast signals to it).
    #[cfg_attr(not(test), allow(dead_code))]
    unique_name: Mutex<String>,
}

impl Status {
    fn set_state(&self, state: WatcherState) {
        self.state.store(state.to_u8(), Ordering::SeqCst);
    }

    fn state(&self) -> WatcherState {
        WatcherState::from_u8(self.state.load(Ordering::SeqCst))
    }

    fn set_session_watched(&self, watched: bool) {
        if watched {
            self.had_session.store(true, Ordering::SeqCst);
        }
        self.session_watched.store(watched, Ordering::SeqCst);
    }
}

/// Clears the status when the watcher thread ends, panic included.
struct RunningGuard(Arc<Status>);

impl Drop for RunningGuard {
    fn drop(&mut self) {
        self.0.set_state(WatcherState::Stopped);
        self.0.sleep_delay.store(false, Ordering::SeqCst);
    }
}

/// Listens to logind's sleep and session-lock signals until dropped.
pub struct DesktopLockWatcher {
    stop: Arc<Stop>,
    status: Arc<Status>,
}

impl std::fmt::Debug for DesktopLockWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DesktopLockWatcher")
            .field("state", &self.state())
            .field("session_watched", &self.watches_session_lock())
            .finish_non_exhaustive()
    }
}

impl DesktopLockWatcher {
    /// Starts watching the system bus. Blocks for at most [`WATCHER_SETUP_TIMEOUT`] (call it on
    /// a background thread, never on the UI thread).
    ///
    /// # Errors
    /// [`LockWatchError`] when there is no system bus or it does not answer; the application
    /// then relies on the idle timers and its own lock action. (Only a bus lost *after* a
    /// successful start is reconnected to.)
    pub fn start(hook: Arc<dyn LockHook>) -> Result<Self, LockWatchError> {
        Self::start_with(
            Target::System,
            SessionChoice::Lookup,
            hook,
            WATCHER_SETUP_TIMEOUT,
            Arc::new(suspended_since_boot),
        )
    }

    /// Whether the session's screen lock was watched before and may not be any more: logind is
    /// back after an outage, but the session could not be looked up again (its retries
    /// included), so the path still watched is the one from an earlier connection and may no
    /// longer exist; or no session is watched at all. Only sleep is then sure to lock the
    /// secrets, a degraded state the application should report. Cleared when a lookup succeeds.
    #[must_use]
    pub fn session_lock_lost(&self) -> bool {
        self.status.had_session.load(Ordering::SeqCst)
            && (!self.status.session_watched.load(Ordering::SeqCst)
                || self.status.session_unconfirmed.load(Ordering::SeqCst))
    }

    /// The current connection's unique bus name.
    #[cfg(test)]
    fn unique_name(&self) -> String {
        self.status
            .unique_name
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Whether the screen lock of the session is watched on the current (or, while
    /// reconnecting, the last) bus connection (`false`: no logind session was found for this
    /// process; only sleep is watched).
    #[must_use]
    pub fn watches_session_lock(&self) -> bool {
        self.status.session_watched.load(Ordering::SeqCst)
    }

    /// What the watcher does now. Read live: it changes when the bus or logind goes away and
    /// comes back.
    #[must_use]
    pub fn state(&self) -> WatcherState {
        self.status.state()
    }

    /// Whether the watcher listens to logind now ([`WatcherState::Watching`]). `false` while
    /// logind is away or the bus connection is being re-established (the hook was then called
    /// with [`LockTrigger::Requested`]), and once the watcher stopped; `true` again once it is
    /// back.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.state() == WatcherState::Watching
    }

    /// Whether a logind sleep delay inhibitor is held now (`false` when logind refused it, and
    /// between `PrepareForSleep(true)` and the resume).
    #[must_use]
    pub fn holds_sleep_delay(&self) -> bool {
        self.status.sleep_delay.load(Ordering::SeqCst)
    }

    fn start_with(
        target: Target,
        session: SessionChoice,
        hook: Arc<dyn LockHook>,
        setup_timeout: Duration,
        suspended: SuspendedFn,
    ) -> Result<Self, LockWatchError> {
        let stop = Arc::new(Stop::default());
        let status = Arc::new(Status::default());
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread_stop = Arc::clone(&stop);
        let thread_status = Arc::clone(&status);
        std::thread::Builder::new()
            .name("ninesql-lock-signals".into())
            .spawn(move || {
                let _running = RunningGuard(Arc::clone(&thread_status));
                zbus::block_on(watch(
                    BusDialer {
                        target,
                        session,
                        last_session: None,
                    },
                    hook,
                    thread_stop,
                    thread_status,
                    suspended,
                    ready_tx,
                ));
            })
            .map_err(|e| LockWatchError::Bus(format!("cannot start the watcher thread: {e}")))?;
        match ready_rx.recv_timeout(setup_timeout) {
            Ok(Ok(())) => Ok(Self { stop, status }),
            Ok(Err(e)) => Err(e),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // The setup races against the stop flag: the thread ends at once.
                stop.stop();
                Err(LockWatchError::Timeout)
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(LockWatchError::Bus(
                "the watcher thread ended during setup".into(),
            )),
        }
    }
}

impl Drop for DesktopLockWatcher {
    fn drop(&mut self) {
        self.stop.stop();
    }
}

#[derive(Debug, Clone)]
enum Target {
    System,
    #[cfg(test)]
    Address(String),
}

fn bus_error(e: zbus::Error) -> LockWatchError {
    LockWatchError::Bus(e.to_string())
}

async fn connect(target: Target) -> Result<Connection, LockWatchError> {
    let builder = match target {
        Target::System => zbus::connection::Builder::system(),
        #[cfg(test)]
        Target::Address(a) => zbus::connection::Builder::address(a.as_str()),
    };
    builder
        .method_timeout(WATCHER_SETUP_TIMEOUT)
        .build()
        .await
        .map_err(bus_error)
}

/// Calls a logind manager method that returns an object path.
async fn manager_path<B>(conn: &Connection, method: &str, body: &B) -> Option<String>
where
    B: serde::Serialize + DynamicType,
{
    let reply = conn
        .call_method(Some(LOGIND), LOGIND_PATH, Some(MANAGER), method, body)
        .await
        .inspect_err(|e| tracing::debug!(method, error = %e, "logind lookup failed"))
        .ok()?;
    let path: OwnedObjectPath = reply.body().deserialize().ok()?;
    (path.as_str() != "/").then(|| path.as_str().to_owned())
}

/// The object path of this process's logind session, if any.
async fn own_session(conn: &Connection) -> Option<String> {
    if let Some(p) = manager_path(conn, "GetSessionByPID", &(std::process::id(),)).await {
        return Some(p);
    }
    if let Ok(id) = std::env::var("XDG_SESSION_ID")
        && !id.is_empty()
        && let Some(p) = manager_path(conn, "GetSession", &(id.as_str(),)).await
    {
        return Some(p);
    }
    // An application started in an `app-*.scope` is outside the session: use the user's
    // display session.
    let uid = rustix::process::getuid().as_raw();
    let user = manager_path(conn, "GetUser", &(uid,)).await?;
    let reply = conn
        .call_method(
            Some(LOGIND),
            user.as_str(),
            Some(PROPERTIES),
            "Get",
            &(USER, "Display"),
        )
        .await
        .inspect_err(|e| tracing::debug!(error = %e, "no display session for the user"))
        .ok()?;
    let value: OwnedVariant = reply.body().deserialize().ok()?;
    let (_id, path): (String, OwnedObjectPath) = value.try_into().ok()?;
    (path.as_str() != "/").then(|| path.as_str().to_owned())
}

/// Takes a logind delay inhibitor for sleep. `None` when logind refuses it (no logind, policy).
// zbus receives the fd without `MSG_CMSG_CLOEXEC`; NineSQL spawns no child process, so it
// cannot leak into one (upstream note, review N5).
async fn take_sleep_delay(conn: &Connection) -> Option<OwnedFd> {
    let reply = conn
        .call_method(
            Some(LOGIND),
            LOGIND_PATH,
            Some(MANAGER),
            "Inhibit",
            &("sleep", "NineSQL", "Lock secrets before sleep", "delay"),
        )
        .await
        .inspect_err(|e| tracing::info!(error = %bus_error_kind(e), "no sleep delay inhibitor"))
        .ok()?;
    reply.body().deserialize::<OwnedFd>().ok()
}

/// Whether the session must be taken as locked, read after an outage: its `LockedHint` is
/// `true`, or it cannot be read (no answer within the method timeout, an error, a malformed
/// reply). Fail-closed: a lock missed during the outage is not ruled out (security review L3).
async fn session_locked_or_unknown(conn: &Connection, path: &str) -> bool {
    let reply = match conn
        .call_method(
            Some(LOGIND),
            path,
            Some(PROPERTIES),
            "Get",
            &(SESSION, "LockedHint"),
        )
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::info!(error = %bus_error_kind(&e), "cannot read the session's LockedHint: taken as locked");
            return true;
        }
    };
    reply
        .body()
        .deserialize::<OwnedVariant>()
        .ok()
        .and_then(|v| bool::try_from(&v).ok())
        .unwrap_or(true)
}

async fn subscribe(
    conn: &Connection,
    rule: Result<MatchRule<'static>, zbus::Error>,
) -> Result<MessageStream, LockWatchError> {
    let rule = rule.map_err(bus_error)?;
    MessageStream::for_match_rule(rule, conn, None)
        .await
        .map_err(bus_error)
}

/// A signal rule restricted to the owner of the logind name.
fn signal_rule(
    interface: &'static str,
    member: &'static str,
    path: &str,
) -> Result<MatchRule<'static>, zbus::Error> {
    MatchRule::builder()
        .msg_type(MessageType::Signal)
        .sender(LOGIND)
        .interface(interface)
        .member(member)
        .path(path.to_owned())
        .build()
        .map(|r| r.to_owned())
}

/// What a received signal means.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Event {
    SleepStarting,
    Resumed,
    SessionLocked,
    /// `org.freedesktop.login1` has a new owner (unique name), or none.
    LogindOwner(Option<String>),
}

const BUS: &str = "org.freedesktop.DBus";

/// The `NameOwnerChanged` rule for the logind name (sent by the bus itself).
fn owner_rule() -> Result<MatchRule<'static>, zbus::Error> {
    MatchRule::builder()
        .msg_type(MessageType::Signal)
        .sender(BUS)
        .interface(BUS)
        .member("NameOwnerChanged")
        .path("/org/freedesktop/DBus")
        .arg(0, LOGIND)
        .build()
        .map(|r| r.to_owned())
}

/// The unique name owning `org.freedesktop.login1` now, if any.
async fn logind_owner(conn: &Connection) -> Option<String> {
    let reply = conn
        .call_method(
            Some(BUS),
            "/org/freedesktop/DBus",
            Some(BUS),
            "GetNameOwner",
            &(LOGIND,),
        )
        .await
        .inspect_err(|e| tracing::info!(error = %bus_error_kind(e), "logind is not on the bus"))
        .ok()?;
    reply.body().deserialize::<String>().ok()
}

/// What a received signal asks for. The bus-side match rules are not enough: a signal sent to
/// this connection by name (a destination header) is delivered whatever the rules say, and the
/// client-side rule check cannot resolve well-known sender names. So every signal is checked
/// here: no destination, and sent by the bus itself (owner changes of the logind name) or by
/// the current unique-name owner of `org.freedesktop.login1`.
fn classify(msg: &Message, session_path: Option<&str>, owner: Option<&str>) -> Option<Event> {
    let header = msg.header();
    if header.message_type() != MessageType::Signal || header.destination().is_some() {
        return None;
    }
    let sender = header.sender()?.as_str();
    let interface = header.interface()?.as_str();
    let member = header.member()?.as_str();
    if sender == BUS && interface == BUS && member == "NameOwnerChanged" {
        let (name, _old, new): (String, String, String) = msg.body().deserialize().ok()?;
        return (name == LOGIND).then(|| Event::LogindOwner((!new.is_empty()).then_some(new)));
    }
    if Some(sender) != owner {
        return None;
    }
    let member = header.member()?.as_str();
    let path = header.path()?.as_str();
    match (interface, member) {
        (MANAGER, "PrepareForSleep") if path == LOGIND_PATH => {
            msg.body().deserialize::<bool>().ok().map(|starting| {
                if starting {
                    Event::SleepStarting
                } else {
                    Event::Resumed
                }
            })
        }
        (SESSION, "Lock") if Some(path) == session_path => Some(Event::SessionLocked),
        (PROPERTIES, "PropertiesChanged") if Some(path) == session_path => {
            type Changed = (String, HashMap<String, OwnedVariant>, Vec<String>);
            let (iface, changed, _invalidated): Changed = msg.body().deserialize().ok()?;
            let locked = changed
                .get("LockedHint")
                .and_then(|v| bool::try_from(v).ok());
            (iface == SESSION && locked == Some(true)).then_some(Event::SessionLocked)
        }
        _ => None,
    }
}

/// First wait before asking logind again for a sleep delay inhibitor it did not hand out.
const INHIBIT_RETRY_FIRST: Duration = if cfg!(test) {
    Duration::from_millis(50)
} else {
    Duration::from_secs(1)
};
/// Longest wait between two inhibitor requests (the back-off doubles up to this bound).
const INHIBIT_RETRY_MAX: Duration = if cfg!(test) {
    Duration::from_millis(400)
} else {
    Duration::from_secs(60)
};

/// A waiter of the [`TimerThread`]: fired, and the task to wake.
type TimerSlot = Arc<Mutex<(bool, Option<Waker>)>>;

#[derive(Default)]
struct TimerState {
    pending: Vec<(Instant, TimerSlot)>,
    shutdown: bool,
}

#[derive(Default)]
struct TimerShared {
    state: Mutex<TimerState>,
    wake: Condvar,
}

/// One long-lived timer thread per watcher, started with it, for every wait of the watcher
/// (the reconnect back-off, the attempt timeout, the inhibitor retry): no thread is spawned per
/// wait, so a failed spawn can never block or speed up the signal loop (security review L4).
/// It ends when dropped. A dropped wait stays in its list until its deadline (at most 60 s).
struct TimerThread {
    shared: Arc<TimerShared>,
    /// The thread, so tests can check it ends on drop.
    #[cfg(test)]
    handle: Option<std::thread::JoinHandle<()>>,
}

impl TimerThread {
    fn start() -> std::io::Result<Self> {
        let shared = Arc::new(TimerShared::default());
        let t = Arc::clone(&shared);
        let _handle = std::thread::Builder::new()
            .name("ninesql-lock-timer".into())
            .spawn(move || timer_loop(&t))?;
        Ok(Self {
            shared,
            #[cfg(test)]
            handle: Some(_handle),
        })
    }

    /// Resolves after `d`.
    fn sleep(&self, d: Duration) -> BoxFuture<'static, ()> {
        let slot: TimerSlot = Arc::default();
        {
            let mut g = self
                .shared
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let deadline = Instant::now().checked_add(d).unwrap_or_else(Instant::now);
            g.pending.push((deadline, Arc::clone(&slot)));
        }
        self.shared.wake.notify_one();
        Box::pin(poll_fn(move |cx| {
            let mut g = slot.lock().unwrap_or_else(PoisonError::into_inner);
            if g.0 {
                Poll::Ready(())
            } else {
                g.1 = Some(cx.waker().clone());
                Poll::Pending
            }
        }))
    }
}

impl Drop for TimerThread {
    fn drop(&mut self) {
        self.shared
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .shutdown = true;
        self.shared.wake.notify_one();
    }
}

fn timer_loop(shared: &TimerShared) {
    let mut g = shared.state.lock().unwrap_or_else(PoisonError::into_inner);
    loop {
        if g.shutdown {
            return;
        }
        let now = Instant::now();
        g.pending.retain(|(deadline, slot)| {
            if *deadline > now {
                return true;
            }
            let mut s = slot.lock().unwrap_or_else(PoisonError::into_inner);
            s.0 = true;
            if let Some(w) = s.1.take() {
                w.wake();
            }
            false
        });
        g = match g.pending.iter().map(|(d, _)| *d).min() {
            Some(next) => {
                shared
                    .wake
                    .wait_timeout(g, next.saturating_duration_since(now))
                    .unwrap_or_else(PoisonError::into_inner)
                    .0
            }
            None => shared.wake.wait(g).unwrap_or_else(PoisonError::into_inner),
        };
    }
}

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
type Retake = BoxFuture<'static, Option<OwnedFd>>;

/// An `Inhibit` call, after `wait` when it is a retry.
fn inhibit_request(conn: &Connection, clocks: &dyn Clocks, wait: Option<Duration>) -> Retake {
    let c = conn.clone();
    let pause = wait.map(|d| clocks.sleep(d));
    Box::pin(async move {
        if let Some(p) = pause {
            p.await;
        }
        take_sleep_delay(&c).await
    })
}

/// A `LockedHint` read of `path` after an outage (`true`: lock).
fn hint_request(conn: &Connection, path: &str) -> BoxFuture<'static, bool> {
    let (c, p) = (conn.clone(), path.to_owned());
    Box::pin(async move { session_locked_or_unknown(&c, &p).await })
}

type Relookup = BoxFuture<'static, Option<(String, Vec<MessageStream>)>>;

/// First wait before looking the session up again after a failed lookup (logind may answer
/// its first calls before it has restored its sessions).
const RELOOKUP_RETRY_FIRST: Duration = if cfg!(test) {
    Duration::from_millis(50)
} else {
    Duration::from_secs(1)
};
/// Retries of a failed session lookup once logind is back (waits: 1 s, then 2 s).
const RELOOKUP_RETRIES: u32 = 2;

/// Looks the session up again and subscribes to its lock signals (logind is back and the
/// session is unknown, or known only from an earlier connection), after `wait` when it is a
/// retry.
fn relookup_request(conn: &Connection, wait: Option<BoxFuture<'static, ()>>) -> Relookup {
    let c = conn.clone();
    Box::pin(async move {
        if let Some(w) = wait {
            w.await;
        }
        let path = own_session(&c).await?;
        match session_streams(&c, &path).await {
            Ok(streams) => Some((path, streams)),
            Err(e) => {
                tracing::info!(
                    kind = e.kind(),
                    "cannot subscribe to the session's lock signals"
                );
                None
            }
        }
    })
}

/// Calls the hook; a panic inside it is logged, never propagated.
fn run_hook(hook: &dyn LockHook, trigger: LockTrigger) {
    if catch_unwind(AssertUnwindSafe(|| hook.lock_now(trigger))).is_err() {
        tracing::error!(trigger = %trigger, "a lock hook panicked");
    }
}

/// Runs `fut` until it ends, or `None` once the watcher is stopped.
async fn until_stopped<F: Future>(stop: &Stop, fut: F) -> Option<F::Output> {
    let mut fut = pin!(fut);
    poll_fn(|cx| {
        if stop.poll_stopped(cx) {
            return Poll::Ready(None);
        }
        fut.as_mut().poll(cx).map(Some)
    })
    .await
}

/// Runs `fut` until it ends (`Some`), or `None` once `timer` fired first.
async fn within<F: Future>(fut: F, mut timer: BoxFuture<'static, ()>) -> Option<F::Output> {
    let mut fut = pin!(fut);
    poll_fn(|cx| {
        if let Poll::Ready(v) = fut.as_mut().poll(cx) {
            return Poll::Ready(Some(v));
        }
        timer.as_mut().poll(cx).map(|()| None)
    })
    .await
}

/// Time spent suspended since boot (seam: a manual value in tests).
type SuspendedFn = Arc<dyn Fn() -> Duration + Send + Sync>;

/// Time source of the watcher: the system's in the application, paused tokio time in tests.
trait Clocks: Send + Sync {
    /// Resolves after `d`.
    fn sleep(&self, d: Duration) -> BoxFuture<'static, ()>;
    /// Time since an arbitrary origin (suspend not counted).
    fn now(&self) -> Duration;
    /// Time the machine has spent suspended since boot.
    fn suspended(&self) -> Duration;
}

struct SystemClocks {
    origin: Instant,
    timer: TimerThread,
    suspended: SuspendedFn,
}

impl Clocks for SystemClocks {
    fn sleep(&self, d: Duration) -> BoxFuture<'static, ()> {
        self.timer.sleep(d)
    }

    fn now(&self) -> Duration {
        self.origin.elapsed()
    }

    fn suspended(&self) -> Duration {
        (self.suspended)()
    }
}

/// `CLOCK_BOOTTIME` minus `CLOCK_MONOTONIC`: the time spent suspended since boot (Linux; zero
/// elsewhere, where there is no logind to watch anyway).
fn suspended_since_boot() -> Duration {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        use rustix::time::{ClockId, clock_gettime};
        let read = |id: ClockId| {
            let ts = clock_gettime(id);
            Duration::new(
                u64::try_from(ts.tv_sec).unwrap_or(0),
                u32::try_from(ts.tv_nsec).unwrap_or(0).min(999_999_999),
            )
        };
        read(ClockId::Boottime).saturating_sub(read(ClockId::Monotonic))
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        Duration::ZERO
    }
}

/// Opens a bus connection with its subscriptions (seam: a fake in tests).
trait Dialer: Send {
    type Link: Link;
    fn dial(&mut self) -> BoxFuture<'_, Result<Self::Link, LockWatchError>>;
}

/// One live bus connection with its subscriptions.
trait Link: Send + 'static {
    /// Publishes what this connection watches (state, sleep delay, session lock).
    fn publish(&self, status: &Status);
    /// Watches until the connection is lost. `after_outage`: it replaces a lost connection, so
    /// a session that locked meanwhile is looked for (`LockedHint`).
    fn run(
        self,
        hook: Arc<dyn LockHook>,
        status: Arc<Status>,
        clocks: Arc<dyn Clocks>,
        after_outage: bool,
    ) -> BoxFuture<'static, ()>;
}

/// The thread body: the first connection (its result goes to `start`), then [`supervise`].
async fn watch(
    mut dialer: BusDialer,
    hook: Arc<dyn LockHook>,
    stop: Arc<Stop>,
    status: Arc<Status>,
    suspended: SuspendedFn,
    ready: mpsc::Sender<Result<(), LockWatchError>>,
) {
    let timer = match TimerThread::start() {
        Ok(t) => t,
        Err(e) => {
            let _ = ready.send(Err(LockWatchError::Bus(format!(
                "cannot start the watcher timer thread: {e}"
            ))));
            return;
        }
    };
    let clocks: Arc<dyn Clocks> = Arc::new(SystemClocks {
        origin: Instant::now(),
        timer,
        suspended,
    });
    let link = match until_stopped(&stop, dialer.dial()).await {
        None => return,
        Some(Ok(l)) => l,
        Some(Err(e)) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    if link.session_path.is_none() {
        tracing::info!("no logind session found: only sleep locks the secrets");
    }
    link.publish(&status);
    if ready.send(Ok(())).is_err() {
        // `start` gave up waiting.
        return;
    }
    supervise(dialer, link, hook, &stop, &status, clocks).await;
}

/// Runs `first`, and after each lost connection locks once and reconnects with a bounded
/// back-off, until stopped.
async fn supervise<D: Dialer>(
    mut dialer: D,
    first: D::Link,
    hook: Arc<dyn LockHook>,
    stop: &Stop,
    status: &Arc<Status>,
    clocks: Arc<dyn Clocks>,
) {
    let mut link = first;
    let mut after_outage = false;
    let mut backoff = RECONNECT_FIRST;
    loop {
        link.publish(status);
        let up_since = clocks.now();
        let run = link.run(
            Arc::clone(&hook),
            Arc::clone(status),
            Arc::clone(&clocks),
            after_outage,
        );
        if until_stopped(stop, run).await.is_none() {
            return;
        }
        status.set_state(WatcherState::Reconnecting);
        status.sleep_delay.store(false, Ordering::SeqCst);
        tracing::warn!("system bus connection lost: locking secrets, then reconnecting");
        run_hook(&*hook, LockTrigger::Requested);
        let suspended_at_loss = clocks.suspended();
        if clocks.now().saturating_sub(up_since) >= RECONNECT_STABLE {
            backoff = RECONNECT_FIRST;
        }
        link = loop {
            tracing::debug!(retry_in = ?backoff, "reconnecting to the system bus");
            if until_stopped(stop, clocks.sleep(backoff)).await.is_none() {
                return;
            }
            backoff = (backoff * 2).min(RECONNECT_MAX);
            let attempt = within(dialer.dial(), clocks.sleep(WATCHER_SETUP_TIMEOUT));
            match until_stopped(stop, attempt).await {
                None => return,
                Some(Some(Ok(l))) => break l,
                Some(Some(Err(e))) => {
                    tracing::debug!(error = %e, "system bus still unavailable");
                }
                Some(None) => tracing::debug!("the system bus did not answer in time"),
            }
        };
        tracing::info!("system bus connection back: sleep and screen lock watched again");
        after_outage = true;
        // A suspend while the bus was away was never announced: lock now, in case secrets were
        // unlocked during the outage.
        if clocks.suspended().saturating_sub(suspended_at_loss) > MISSED_SLEEP_SLACK {
            tracing::info!("the machine slept while the system bus was away: locking secrets");
            run_hook(&*hook, LockTrigger::Sleep);
        }
    }
}

/// Dials the system bus (or a test bus) and subscribes.
struct BusDialer {
    target: Target,
    session: SessionChoice,
    /// The session found on an earlier connection, used when the lookup fails on a redial
    /// (logind not back yet), so the screen lock stays watched (security review M1).
    last_session: Option<String>,
}

impl Dialer for BusDialer {
    type Link = BusLink;

    fn dial(&mut self) -> BoxFuture<'_, Result<BusLink, LockWatchError>> {
        Box::pin(async move {
            let link = dial_bus(
                self.target.clone(),
                self.session.clone(),
                self.last_session.clone(),
            )
            .await?;
            if link.session_path.is_some() {
                self.last_session.clone_from(&link.session_path);
            }
            Ok(link)
        })
    }
}

/// A live system-bus connection with its subscriptions.
struct BusLink {
    conn: Connection,
    session_path: Option<String>,
    streams: Vec<MessageStream>,
    owner: Option<String>,
    inhibitor: Option<OwnedFd>,
    unique: String,
    /// The session is looked up through logind (not a fixed test path).
    lookup: bool,
    /// `session_path` comes from an earlier connection: looked up again once logind answers.
    session_stale: bool,
}

/// Subscriptions to the `Lock` signal and the `LockedHint` changes of the session at `path`.
async fn session_streams(
    conn: &Connection,
    path: &str,
) -> Result<Vec<MessageStream>, LockWatchError> {
    let props = MatchRule::builder()
        .msg_type(MessageType::Signal)
        .sender(LOGIND)
        .interface(PROPERTIES)
        .member("PropertiesChanged")
        .path(path.to_owned())
        .arg(0, SESSION)
        .build()
        .map(|r| r.to_owned());
    Ok(vec![
        subscribe(conn, signal_rule(SESSION, "Lock", path)).await?,
        subscribe(conn, props).await?,
    ])
}

/// The owner and `PrepareForSleep` streams come first in [`BusLink::streams`]; the session's
/// streams follow.
const SESSION_STREAMS_AT: usize = 2;

async fn dial_bus(
    target: Target,
    session: SessionChoice,
    last_session: Option<String>,
) -> Result<BusLink, LockWatchError> {
    let conn = connect(target).await?;
    let lookup = matches!(session, SessionChoice::Lookup);
    let (session_path, session_stale) = match session {
        SessionChoice::Lookup => match own_session(&conn).await {
            Some(p) => (Some(p), false),
            None if last_session.is_some() => {
                tracing::info!("session lookup failed: watching the session found before");
                (last_session, true)
            }
            None => (None, false),
        },
        #[cfg(test)]
        SessionChoice::Path(p) => (Some(p), false),
    };
    // The owner stream comes first and is always polled first: after a logind restart, the
    // `NameOwnerChanged` naming the new owner is handled before any signal the new owner
    // sends, so its first real signal is not dropped as coming from an unknown sender.
    // Subscribed before the owner lookup below, so an owner change in between is not missed.
    let mut streams = vec![
        subscribe(&conn, owner_rule()).await?,
        subscribe(&conn, signal_rule(MANAGER, "PrepareForSleep", LOGIND_PATH)).await?,
    ];
    if let Some(p) = &session_path {
        streams.extend(session_streams(&conn, p).await?);
    }
    let owner = logind_owner(&conn).await;
    let inhibitor = if owner.is_some() {
        take_sleep_delay(&conn).await
    } else {
        None
    };
    let unique = conn
        .unique_name()
        .map(|n| n.as_str().to_owned())
        .unwrap_or_default();
    Ok(BusLink {
        conn,
        session_path,
        streams,
        owner,
        inhibitor,
        unique,
        lookup,
        session_stale,
    })
}

impl Link for BusLink {
    fn publish(&self, status: &Status) {
        status.set_state(if self.owner.is_some() {
            WatcherState::Watching
        } else {
            WatcherState::LogindAbsent
        });
        status
            .sleep_delay
            .store(self.inhibitor.is_some(), Ordering::SeqCst);
        status.set_session_watched(self.session_path.is_some());
        status.session_unconfirmed.store(false, Ordering::SeqCst);
        self.unique.clone_into(
            &mut status
                .unique_name
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
    }

    fn run(
        self,
        hook: Arc<dyn LockHook>,
        status: Arc<Status>,
        clocks: Arc<dyn Clocks>,
        after_outage: bool,
    ) -> BoxFuture<'static, ()> {
        Box::pin(self.watch_signals(hook, status, clocks, after_outage))
    }
}

impl BusLink {
    /// The signal loop of one connection; returns when the connection is lost (the stop flag is
    /// checked by [`supervise`]). The inhibitor is released when this future ends or is dropped.
    async fn watch_signals(
        self,
        hook: Arc<dyn LockHook>,
        status: Arc<Status>,
        clocks: Arc<dyn Clocks>,
        after_outage: bool,
    ) {
        let Self {
            conn,
            mut session_path,
            mut streams,
            mut owner,
            mut inhibitor,
            unique: _,
            lookup,
            mut session_stale,
        } = self;
        // The `Inhibit` call after a resume or an owner change runs alongside the streams, so
        // signals are handled while logind answers. A refused or unanswered call is retried with
        // a bounded back-off while logind is on the bus, no inhibitor is held and no sleep is
        // under way (review P1-D wiring, L1).
        let mut asleep = false;
        let mut backoff = INHIBIT_RETRY_FIRST;
        let mut retake: Option<Retake> = None;
        if inhibitor.is_none() && owner.is_some() {
            retake = Some(inhibit_request(&conn, &*clocks, Some(backoff)));
            backoff = (backoff * 2).min(INHIBIT_RETRY_MAX);
        }
        // A screen lock missed while not watched is read from `LockedHint`, once the session is
        // known for sure (looked up again when it is unknown or from an earlier connection).
        let mut hint: Option<BoxFuture<'static, bool>> = None;
        let mut relookup: Option<Relookup> = None;
        // Failed lookups since logind (re)appeared (re-review N2).
        let mut relookup_failures: u32 = 0;
        let check_missed_lock = |session_path: &Option<String>, session_stale: bool| {
            if lookup && (session_path.is_none() || session_stale) {
                (None, Some(relookup_request(&conn, None)))
            } else {
                (
                    session_path.as_deref().map(|p| hint_request(&conn, p)),
                    None,
                )
            }
        };
        if after_outage && owner.is_some() {
            (hint, relookup) = check_missed_lock(&session_path, session_stale);
        }
        // Suspended time when logind left the bus, or when this connection found it absent
        // (security review L2): a suspend in that window was never announced.
        let mut suspended_at_logind_loss: Option<Duration> =
            owner.is_none().then(|| clocks.suspended());
        loop {
            let next = poll_fn(|cx| {
                if let Some(f) = retake.as_mut()
                    && let Poll::Ready(fd) = f.as_mut().poll(cx)
                {
                    return Poll::Ready(Next::Inhibitor(fd));
                }
                if let Some(f) = relookup.as_mut()
                    && let Poll::Ready(found) = f.as_mut().poll(cx)
                {
                    return Poll::Ready(Next::Session(found));
                }
                if let Some(f) = hint.as_mut()
                    && let Poll::Ready(locked) = f.as_mut().poll(cx)
                {
                    return Poll::Ready(Next::LockedHint(locked));
                }
                for s in &mut streams {
                    if let Poll::Ready(m) = Pin::new(s).poll_next(cx) {
                        return Poll::Ready(Next::Message(m));
                    }
                }
                Poll::Pending
            })
            .await;
            match next {
                Next::Inhibitor(fd) => {
                    retake = None;
                    inhibitor = fd;
                    status
                        .sleep_delay
                        .store(inhibitor.is_some(), Ordering::SeqCst);
                    if inhibitor.is_some() {
                        backoff = INHIBIT_RETRY_FIRST;
                    } else if owner.is_some() && !asleep {
                        tracing::debug!(retry_in = ?backoff, "sleep delay inhibitor refused: retrying");
                        retake = Some(inhibit_request(&conn, &*clocks, Some(backoff)));
                        backoff = (backoff * 2).min(INHIBIT_RETRY_MAX);
                    }
                }
                Next::Session(found) => {
                    relookup = None;
                    if let Some((path, new_streams)) = found {
                        streams.truncate(SESSION_STREAMS_AT);
                        streams.extend(new_streams);
                        session_path = Some(path);
                        session_stale = false;
                        relookup_failures = 0;
                        status.set_session_watched(true);
                        status.session_unconfirmed.store(false, Ordering::SeqCst);
                        hint = session_path.as_deref().map(|p| hint_request(&conn, p));
                    } else {
                        relookup_failures += 1;
                        // Reported at once, cleared if a retry succeeds (re-review N1).
                        status.session_unconfirmed.store(true, Ordering::SeqCst);
                        if relookup_failures == 1 {
                            // The earlier session (if any) stays watched; its hint is read
                            // anyway, and an unreadable hint locks.
                            hint = session_path.as_deref().map(|p| hint_request(&conn, p));
                        }
                        if relookup_failures <= RELOOKUP_RETRIES {
                            let wait = RELOOKUP_RETRY_FIRST * (1 << (relookup_failures - 1));
                            tracing::debug!(retry_in = ?wait, "session lookup failed: retrying");
                            relookup = Some(relookup_request(&conn, Some(clocks.sleep(wait))));
                        } else {
                            tracing::warn!(
                                "the session could not be found again: its screen lock may not be watched"
                            );
                        }
                    }
                }
                Next::LockedHint(locked) => {
                    hint = None;
                    if locked {
                        tracing::info!(
                            "the session may have locked while it was not watched: locking secrets"
                        );
                        run_hook(&*hook, LockTrigger::SessionLock);
                    }
                }
                Next::Message(Some(Ok(msg))) => {
                    match classify(&msg, session_path.as_deref(), owner.as_deref()) {
                        Some(Event::SleepStarting) => {
                            run_hook(&*hook, LockTrigger::Sleep);
                            // Secrets are gone: let the suspend go on.
                            asleep = true;
                            retake = None;
                            inhibitor = None;
                            status.sleep_delay.store(false, Ordering::SeqCst);
                        }
                        Some(Event::Resumed) => {
                            asleep = false;
                            if inhibitor.is_none() {
                                // A pending back-off retry is replaced by an immediate request.
                                backoff = INHIBIT_RETRY_FIRST;
                                retake = Some(inhibit_request(&conn, &*clocks, None));
                            }
                        }
                        Some(Event::SessionLocked) => run_hook(&*hook, LockTrigger::SessionLock),
                        Some(Event::LogindOwner(new)) => {
                            tracing::info!(present = new.is_some(), "logind owner changed");
                            if new != owner {
                                // An inhibitor taken from the previous owner protects nothing any
                                // more: drop it, and take one from the new logind when it appears.
                                retake = None;
                                inhibitor = None;
                                asleep = false;
                                backoff = INHIBIT_RETRY_FIRST;
                                status.sleep_delay.store(false, Ordering::SeqCst);
                                if new.is_some() {
                                    status.set_state(WatcherState::Watching);
                                    retake = Some(inhibit_request(&conn, &*clocks, None));
                                    if let Some(at) = suspended_at_logind_loss.take()
                                        && clocks.suspended().saturating_sub(at)
                                            > MISSED_SLEEP_SLACK
                                    {
                                        tracing::info!(
                                            "the machine slept while logind was away: locking secrets"
                                        );
                                        run_hook(&*hook, LockTrigger::Sleep);
                                    }
                                    // A lock missed while logind was away or restarting.
                                    relookup_failures = 0;
                                    (hint, relookup) =
                                        check_missed_lock(&session_path, session_stale);
                                } else {
                                    // Nothing is watched until logind returns: fail closed.
                                    hint = None;
                                    relookup = None;
                                    status.set_state(WatcherState::LogindAbsent);
                                    suspended_at_logind_loss = Some(clocks.suspended());
                                    if owner.is_some() {
                                        tracing::warn!(
                                            "logind left the system bus: locking secrets"
                                        );
                                        run_hook(&*hook, LockTrigger::Requested);
                                    }
                                }
                            }
                            owner = new;
                        }
                        None => {}
                    }
                }
                Next::Message(Some(Err(e))) => {
                    tracing::debug!(error = %e, "bad message on the system bus");
                }
                Next::Message(None) => return,
            }
        }
    }
}

/// What woke the signal loop.
enum Next {
    Inhibitor(Option<OwnedFd>),
    Session(Option<(String, Vec<MessageStream>)>),
    LockedHint(bool),
    Message(Option<zbus::Result<Message>>),
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stderr)] // Tests may panic (AGENTS.md).
mod tests {
    use std::io::{BufRead, BufReader, Read};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::process::{Child, Command, Stdio};
    use std::sync::Mutex as StdMutex;
    use std::time::Instant;

    use super::*;

    /// Takes one unit from `counter` if it is above zero. Written with
    /// `compare_exchange` so it builds on every supported toolchain
    /// (`fetch_update` is deprecated on newer ones, `try_update` is too new for the MSRV).
    fn take_one(counter: &std::sync::atomic::AtomicUsize) -> bool {
        let mut current = counter.load(Ordering::SeqCst);
        while let Some(next) = current.checked_sub(1) {
            match counter.compare_exchange(current, next, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => return true,
                Err(seen) => current = seen,
            }
        }
        false
    }

    /// Records the triggers it receives; panics on the first call when asked to.
    #[derive(Default)]
    struct Recorder {
        got: StdMutex<Vec<LockTrigger>>,
        panic_once: AtomicBool,
    }

    impl LockHook for Recorder {
        fn lock_now(&self, trigger: LockTrigger) {
            self.got.lock().unwrap().push(trigger);
            assert!(
                !self.panic_once.swap(false, Ordering::SeqCst),
                "simulated hook failure"
            );
        }
    }

    impl Recorder {
        fn got(&self) -> Vec<LockTrigger> {
            self.got.lock().unwrap().clone()
        }

        fn wait_for(&self, n: usize) -> Vec<LockTrigger> {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let got = self.got();
                if got.len() >= n || Instant::now() > deadline {
                    return got;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }

    fn wait(what: &str, mut ok: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !ok() {
            assert!(Instant::now() < deadline, "timed out waiting: {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// A private bus (`dbus-daemon --session`), killed on drop.
    struct Bus {
        child: Child,
        address: String,
    }

    impl Drop for Bus {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    /// `None` when `dbus-daemon` is not installed: the bus tests are skipped, unless
    /// `NINESQL_REQUIRE_DBUS=1` (CI) makes that a failure.
    fn private_bus() -> Option<Bus> {
        private_bus_at(None)
    }

    /// A private bus listening on `socket` when given (so it can be restarted at the same
    /// address). `None` (skip) or a panic (`NINESQL_REQUIRE_DBUS=1`) without `dbus-daemon`.
    fn private_bus_at(socket: Option<&std::path::Path>) -> Option<Bus> {
        let bus = spawn_bus(socket);
        assert!(
            bus.is_some() || std::env::var("NINESQL_REQUIRE_DBUS").as_deref() != Ok("1"),
            "NINESQL_REQUIRE_DBUS=1 but dbus-daemon could not be started"
        );
        bus
    }

    fn spawn_bus(socket: Option<&std::path::Path>) -> Option<Bus> {
        let mut args = vec![
            "--session".to_owned(),
            "--nofork".to_owned(),
            "--print-address=1".to_owned(),
        ];
        if let Some(p) = socket {
            args.push(format!("--address=unix:path={}", p.display()));
        }
        let mut child = Command::new("dbus-daemon")
            .args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let mut line = String::new();
        BufReader::new(child.stdout.take()?)
            .read_line(&mut line)
            .ok()?;
        let address = line.trim().to_owned();
        if address.is_empty() {
            let _ = child.kill();
            return None;
        }
        Some(Bus { child, address })
    }

    fn connect_to(address: &str) -> Connection {
        zbus::block_on(zbus::connection::Builder::address(address).build()).unwrap()
    }

    /// A stand-in for logind on the private bus: owns `org.freedesktop.login1`, answers
    /// `Inhibit` with one end of a socket pair (keeping the other end to see when the watcher
    /// closes it), refuses the session lookups by PID and id, and answers the user lookup.
    struct FakeLogind {
        conn: Connection,
        inhibitors: Arc<StdMutex<Vec<UnixStream>>>,
        /// The session's `LockedHint`, as answered to `Get` ([`Hint`] as `u8`).
        hint: Arc<AtomicU8>,
        /// `GetUser` calls received (the session lookup through the user).
        user_lookups: Arc<std::sync::atomic::AtomicUsize>,
    }

    /// How the fake answers a `Get` of the session's `LockedHint`.
    #[derive(Clone, Copy)]
    #[repr(u8)]
    enum Hint {
        Unlocked = 0,
        Locked = 1,
        /// An error reply.
        Fails = 2,
    }

    const OUR_SESSION: &str = "/org/freedesktop/login1/session/_31";
    const OTHER_SESSION: &str = "/org/freedesktop/login1/session/_32";
    const USER_PATH: &str = "/org/freedesktop/login1/user/_1000";

    impl FakeLogind {
        fn start(address: &str) -> Self {
            Self::start_refusing(address, 0)
        }

        /// Like [`Self::start`], refusing the first `refuse` `Inhibit` calls.
        fn start_refusing(address: &str, refuse: usize) -> Self {
            Self::start_with(address, refuse, Hint::Unlocked)
        }

        /// Like [`Self::start_refusing`], answering `LockedHint` as `hint` says.
        fn start_with(address: &str, refuse: usize, hint: Hint) -> Self {
            Self::start_full(address, refuse, hint, 0)
        }

        /// Like [`Self::start_with`], failing the first `fail_lookups` `GetUser` calls (the
        /// session cannot be found).
        fn start_full(address: &str, refuse: usize, hint: Hint, fail_lookups: usize) -> Self {
            let user_lookups = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let lookups = Arc::clone(&user_lookups);
            let lookup_failures = Arc::new(std::sync::atomic::AtomicUsize::new(fail_lookups));
            let hint_mode = Arc::new(AtomicU8::new(hint as u8));
            let hint = Arc::clone(&hint_mode);
            let conn = connect_to(address);
            let refusals = Arc::new(std::sync::atomic::AtomicUsize::new(refuse));
            let inhibitors: Arc<StdMutex<Vec<UnixStream>>> = Arc::default();
            let (c, inh, refusals) = (conn.clone(), Arc::clone(&inhibitors), Arc::clone(&refusals));
            let (ready_tx, ready_rx) = mpsc::channel();
            std::thread::spawn(move || {
                zbus::block_on(async move {
                    let mut stream = MessageStream::from(&c);
                    let _ = ready_tx.send(());
                    while let Some(Ok(msg)) =
                        poll_fn(|cx| Pin::new(&mut stream).poll_next(cx)).await
                    {
                        let h = msg.header();
                        if h.message_type() != MessageType::MethodCall {
                            continue;
                        }
                        match h.member().map(|m| m.as_str().to_owned()).as_deref() {
                            Some("Inhibit") if take_one(&refusals) => {
                                c.reply_error(
                                    &h,
                                    "org.freedesktop.DBus.Error.AccessDenied",
                                    &"refused",
                                )
                                .await
                                .unwrap();
                            }
                            Some("Inhibit") => {
                                let (ours, theirs) = UnixStream::pair().unwrap();
                                let fd = OwnedFd::from(std::os::fd::OwnedFd::from(theirs));
                                c.reply(&h, &fd).await.unwrap();
                                drop(fd);
                                inh.lock().unwrap().push(ours);
                            }
                            Some("GetSessionByPID" | "GetSession") => {
                                c.reply_error(
                                    &h,
                                    "org.freedesktop.login1.NoSuchSession",
                                    &"no session",
                                )
                                .await
                                .unwrap();
                            }
                            Some("GetUser")
                                if {
                                    lookups.fetch_add(1, Ordering::SeqCst);
                                    take_one(&lookup_failures)
                                } =>
                            {
                                c.reply_error(&h, "org.freedesktop.login1.NoSuchUser", &"no user")
                                    .await
                                    .unwrap();
                            }
                            Some("GetUser") => {
                                let p = OwnedObjectPath::try_from(USER_PATH).unwrap();
                                c.reply(&h, &p).await.unwrap();
                            }
                            Some("Get")
                                if msg
                                    .body()
                                    .deserialize::<(String, String)>()
                                    .is_ok_and(|(i, p)| i == SESSION && p == "LockedHint") =>
                            {
                                match hint.load(Ordering::SeqCst) {
                                    2 => c
                                        .reply_error(
                                            &h,
                                            "org.freedesktop.DBus.Error.Failed",
                                            &"unavailable",
                                        )
                                        .await
                                        .unwrap(),
                                    v => {
                                        let v = zbus::Variant::from(v == 1);
                                        c.reply(&h, &v).await.unwrap();
                                    }
                                }
                            }
                            Some("Get") => {
                                let p = OwnedObjectPath::try_from(OUR_SESSION).unwrap();
                                let v = zbus::Variant::from(("31".to_owned(), p));
                                c.reply(&h, &v).await.unwrap();
                            }
                            _ => {}
                        }
                    }
                });
            });
            ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            // The name is taken only once the message stream above listens: a watcher reacting
            // to the owner change at once (`Inhibit`) must find the fake answering
            // (review P1-D wiring, M1).
            zbus::block_on(conn.request_name(LOGIND)).unwrap();
            Self {
                conn,
                inhibitors,
                hint: hint_mode,
                user_lookups,
            }
        }

        fn inhibit_calls(&self) -> usize {
            self.inhibitors.lock().unwrap().len()
        }

        /// Whether the watcher closed its end of the `n`-th inhibitor.
        fn inhibitor_released(&self, n: usize) -> bool {
            let inh = self.inhibitors.lock().unwrap();
            let Some(s) = inh.get(n) else { return false };
            s.set_nonblocking(true).unwrap();
            let mut buf = [0u8; 1];
            matches!((&*s).read(&mut buf), Ok(0))
        }

        fn emit(&self, path: &str, interface: &str, member: &str, body: Option<bool>) {
            emit_on(&self.conn, path, interface, member, body);
        }

        fn locked_hint(&self, path: &str, iface: &str, value: bool) {
            let mut changed = HashMap::new();
            changed.insert("LockedHint", zbus::Variant::from(value));
            zbus::block_on(self.conn.emit_signal(
                None::<&str>,
                path,
                PROPERTIES,
                "PropertiesChanged",
                &(iface, changed, Vec::<String>::new()),
            ))
            .unwrap();
        }
    }

    fn emit_on(conn: &Connection, path: &str, interface: &str, member: &str, body: Option<bool>) {
        zbus::block_on(async {
            match body {
                Some(b) => conn
                    .emit_signal(None::<&str>, path, interface, member, &(b,))
                    .await
                    .unwrap(),
                None => conn
                    .emit_signal(None::<&str>, path, interface, member, &())
                    .await
                    .unwrap(),
            }
        });
    }

    fn watcher(bus: &Bus, session: SessionChoice, hook: Arc<dyn LockHook>) -> DesktopLockWatcher {
        DesktopLockWatcher::start_with(
            Target::Address(bus.address.clone()),
            session,
            hook,
            WATCHER_SETUP_TIMEOUT,
            Arc::new(suspended_since_boot),
        )
        .unwrap()
    }

    #[test]
    fn sleep_and_own_session_lock_trigger_the_hook() {
        let Some(bus) = private_bus() else {
            eprintln!("dbus-daemon not available: skipped");
            return;
        };
        let logind = FakeLogind::start(&bus.address);
        let recorder = Arc::new(Recorder::default());
        let w = watcher(
            &bus,
            SessionChoice::Path(OUR_SESSION.into()),
            recorder.clone(),
        );
        assert!(w.watches_session_lock());
        assert!(w.is_running());

        // Ignored: the resume, another session's lock, other members, wrong paths, and
        // signals from a peer that does not own the logind name.
        logind.emit(LOGIND_PATH, MANAGER, "PrepareForSleep", Some(false));
        logind.emit(OTHER_SESSION, SESSION, "Lock", None);
        logind.emit(OUR_SESSION, SESSION, "Unlock", None);
        logind.emit("/elsewhere", MANAGER, "PrepareForSleep", Some(true));
        let forger = connect_to(&bus.address);
        emit_on(&forger, LOGIND_PATH, MANAGER, "PrepareForSleep", Some(true));
        emit_on(&forger, OUR_SESSION, SESSION, "Lock", None);
        // Acted on.
        logind.emit(LOGIND_PATH, MANAGER, "PrepareForSleep", Some(true));
        logind.emit(OUR_SESSION, SESSION, "Lock", None);

        let got = recorder.wait_for(2);
        assert_eq!(got, [LockTrigger::Sleep, LockTrigger::SessionLock]);
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(recorder.got().len(), 2, "nothing else triggered");

        // Once dropped, nothing reaches the hook.
        drop(w);
        std::thread::sleep(Duration::from_millis(100));
        logind.emit(LOGIND_PATH, MANAGER, "PrepareForSleep", Some(true));
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(recorder.got().len(), 2);
    }

    #[test]
    fn a_unicast_forged_signal_is_ignored_and_the_delay_kept() {
        let Some(bus) = private_bus() else {
            eprintln!("dbus-daemon not available: skipped");
            return;
        };
        let logind = FakeLogind::start(&bus.address);
        let recorder = Arc::new(Recorder::default());
        let w = watcher(
            &bus,
            SessionChoice::Path(OUR_SESSION.into()),
            recorder.clone(),
        );
        assert!(w.holds_sleep_delay());
        let name = w.unique_name();
        assert!(!name.is_empty());
        // Sent to the watcher by name: the bus delivers it whatever the match rules say.
        let forger = connect_to(&bus.address);
        let to = Some(name.as_str());
        zbus::block_on(async {
            forger
                .emit_signal(to, LOGIND_PATH, MANAGER, "PrepareForSleep", &(true,))
                .await
                .unwrap();
            forger
                .emit_signal(to, OUR_SESSION, SESSION, "Lock", &())
                .await
                .unwrap();
            // A forged owner change, then a broadcast that would match the forger.
            forger
                .emit_signal(
                    to,
                    "/org/freedesktop/DBus",
                    BUS,
                    "NameOwnerChanged",
                    &(LOGIND, "", forger.unique_name().unwrap().as_str()),
                )
                .await
                .unwrap();
        });
        emit_on(&forger, LOGIND_PATH, MANAGER, "PrepareForSleep", Some(true));
        // The real logind signal sent from the same name owner, unicast, is refused too.
        zbus::block_on(
            logind
                .conn
                .emit_signal(to, OUR_SESSION, SESSION, "Lock", &()),
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(400));
        assert!(recorder.got().is_empty(), "{:?}", recorder.got());
        assert!(w.holds_sleep_delay());
        assert!(!logind.inhibitor_released(0));
        // The genuine broadcast still works.
        logind.emit(OUR_SESSION, SESSION, "Lock", None);
        assert_eq!(recorder.wait_for(1), [LockTrigger::SessionLock]);
    }

    #[test]
    fn a_new_logind_owner_is_followed() {
        let Some(bus) = private_bus() else {
            eprintln!("dbus-daemon not available: skipped");
            return;
        };
        let first = FakeLogind::start(&bus.address);
        let recorder = Arc::new(Recorder::default());
        let w = watcher(
            &bus,
            SessionChoice::Path(OUR_SESSION.into()),
            recorder.clone(),
        );
        // logind restarts: the old owner leaves, a new connection takes the name.
        zbus::block_on(first.conn.release_name(LOGIND)).unwrap();
        // logind leaving locks once (fail-closed) and is reported.
        assert_eq!(recorder.wait_for(1), [LockTrigger::Requested]);
        wait("logind absent", || w.state() == WatcherState::LogindAbsent);
        assert!(!w.is_running());
        let second = FakeLogind::start(&bus.address);
        wait("watching again", || w.is_running());
        std::thread::sleep(Duration::from_millis(200));
        first.emit(OUR_SESSION, SESSION, "Lock", None);
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(
            recorder.got(),
            [LockTrigger::Requested],
            "the old owner is not trusted any more"
        );
        second.emit(OUR_SESSION, SESSION, "Lock", None);
        assert_eq!(
            recorder.wait_for(2),
            [LockTrigger::Requested, LockTrigger::SessionLock]
        );
        drop(w);
    }

    /// ROADMAP P1-D review (lock signals, Low): after a logind restart, the delay inhibitor is
    /// taken again from the new logind, and the new owner's first signal, sent right after it
    /// took the name, is honoured. The drop this guards against (the signal polled before the
    /// `NameOwnerChanged`, fixed by polling the owner stream first) needs both messages queued
    /// before the watcher wakes up, which this test cannot force: it does not fail on the old
    /// polling order, it only checks the behaviour end to end.
    #[test]
    fn a_restarted_logind_gets_a_new_inhibitor_and_its_first_signal_counts() {
        let Some(bus) = private_bus() else {
            eprintln!("dbus-daemon not available: skipped");
            return;
        };
        let first = FakeLogind::start(&bus.address);
        let recorder = Arc::new(Recorder::default());
        let w = watcher(
            &bus,
            SessionChoice::Path(OUR_SESSION.into()),
            recorder.clone(),
        );
        assert_eq!(first.inhibit_calls(), 1);
        assert!(w.holds_sleep_delay());

        // logind goes away: the inhibitor it handed out protects nothing any more.
        zbus::block_on(first.conn.release_name(LOGIND)).unwrap();
        wait("delay flag cleared", || !w.holds_sleep_delay());
        wait("old inhibitor dropped", || first.inhibitor_released(0));

        // A new logind takes the name and at once announces a suspend.
        let second = FakeLogind::start(&bus.address);
        second.emit(LOGIND_PATH, MANAGER, "PrepareForSleep", Some(true));
        assert_eq!(
            recorder.wait_for(2),
            [LockTrigger::Requested, LockTrigger::Sleep]
        );
        // Resume: an inhibitor is (re)taken from the new logind.
        second.emit(LOGIND_PATH, MANAGER, "PrepareForSleep", Some(false));
        wait("inhibitor taken from the new logind", || {
            second.inhibit_calls() >= 1
        });
        wait("delay flag set", || w.holds_sleep_delay());
        assert_eq!(first.inhibit_calls(), 1);
        drop(w);
    }

    /// A logind that reappears without any sleep cycle still gets asked for an inhibitor.
    #[test]
    fn a_reappearing_logind_is_asked_for_an_inhibitor() {
        let Some(bus) = private_bus() else {
            eprintln!("dbus-daemon not available: skipped");
            return;
        };
        let first = FakeLogind::start(&bus.address);
        let recorder = Arc::new(Recorder::default());
        let w = watcher(
            &bus,
            SessionChoice::Path(OUR_SESSION.into()),
            recorder.clone(),
        );
        zbus::block_on(first.conn.release_name(LOGIND)).unwrap();
        wait("delay flag cleared", || !w.holds_sleep_delay());
        let second = FakeLogind::start(&bus.address);
        wait("inhibitor taken from the new logind", || {
            second.inhibit_calls() == 1
        });
        wait("delay flag set", || w.holds_sleep_delay());
        assert!(w.is_running());
        assert_eq!(recorder.got(), [LockTrigger::Requested]);
        drop(w);
        wait("released on drop", || second.inhibitor_released(0));
    }

    /// Review P1-D wiring, L1: a refused re-take after an owner change is retried.
    #[test]
    fn a_refused_inhibitor_is_asked_again_with_back_off() {
        let Some(bus) = private_bus() else {
            eprintln!("dbus-daemon not available: skipped");
            return;
        };
        let first = FakeLogind::start(&bus.address);
        let recorder = Arc::new(Recorder::default());
        let w = watcher(
            &bus,
            SessionChoice::Path(OUR_SESSION.into()),
            recorder.clone(),
        );
        zbus::block_on(first.conn.release_name(LOGIND)).unwrap();
        wait("delay flag cleared", || !w.holds_sleep_delay());
        let second = FakeLogind::start_refusing(&bus.address, 3);
        wait("inhibitor taken after the refusals", || {
            second.inhibit_calls() == 1
        });
        wait("delay flag set", || w.holds_sleep_delay());
        std::thread::sleep(Duration::from_millis(600));
        assert_eq!(second.inhibit_calls(), 1, "no request once one is held");
        assert_eq!(recorder.got(), [LockTrigger::Requested]);
        drop(w);
    }

    #[test]
    fn the_sleep_delay_is_held_released_after_the_hook_and_taken_again() {
        let Some(bus) = private_bus() else {
            eprintln!("dbus-daemon not available: skipped");
            return;
        };
        let logind = FakeLogind::start(&bus.address);
        let recorder = Arc::new(Recorder::default());
        let w = watcher(
            &bus,
            SessionChoice::Path(OUR_SESSION.into()),
            recorder.clone(),
        );
        assert_eq!(logind.inhibit_calls(), 1);
        assert!(w.holds_sleep_delay());
        assert!(!logind.inhibitor_released(0));

        logind.emit(LOGIND_PATH, MANAGER, "PrepareForSleep", Some(true));
        wait("inhibitor released", || logind.inhibitor_released(0));
        // The hook ran before the inhibitor was released.
        assert_eq!(recorder.got(), [LockTrigger::Sleep]);
        wait("delay flag cleared", || !w.holds_sleep_delay());

        logind.emit(LOGIND_PATH, MANAGER, "PrepareForSleep", Some(false));
        wait("inhibitor taken again", || logind.inhibit_calls() == 2);
        wait("delay flag set", || w.holds_sleep_delay());
        assert!(!logind.inhibitor_released(1));
        drop(w);
        wait("released on drop", || logind.inhibitor_released(1));
    }

    #[test]
    fn locked_hint_locks_and_the_session_is_found_through_the_user() {
        let Some(bus) = private_bus() else {
            eprintln!("dbus-daemon not available: skipped");
            return;
        };
        let logind = FakeLogind::start(&bus.address);
        let recorder = Arc::new(Recorder::default());
        // PID and session id lookups fail: the user's display session is used.
        let w = watcher(&bus, SessionChoice::Lookup, recorder.clone());
        assert!(w.watches_session_lock());

        logind.locked_hint(OUR_SESSION, SESSION, false);
        logind.locked_hint(OTHER_SESSION, SESSION, true);
        logind.locked_hint(OUR_SESSION, USER, true);
        logind.locked_hint(OUR_SESSION, SESSION, true);
        assert_eq!(recorder.wait_for(1), [LockTrigger::SessionLock]);
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(recorder.got().len(), 1);
    }

    #[test]
    fn a_panicking_hook_does_not_stop_the_watcher_and_bus_loss_fails_closed() {
        let Some(bus) = private_bus() else {
            eprintln!("dbus-daemon not available: skipped");
            return;
        };
        let logind = FakeLogind::start(&bus.address);
        let recorder = Arc::new(Recorder::default());
        recorder.panic_once.store(true, Ordering::SeqCst);
        let w = watcher(
            &bus,
            SessionChoice::Path(OUR_SESSION.into()),
            recorder.clone(),
        );
        logind.emit(OUR_SESSION, SESSION, "Lock", None);
        logind.emit(OUR_SESSION, SESSION, "Lock", None);
        assert_eq!(
            recorder.wait_for(2),
            [LockTrigger::SessionLock, LockTrigger::SessionLock]
        );
        assert!(w.is_running());

        drop(logind);
        drop(bus);
        let got = recorder.wait_for(3);
        assert_eq!(got.get(2), Some(&LockTrigger::Requested), "{got:?}");
        wait("watcher reconnecting", || {
            w.state() == WatcherState::Reconnecting
        });
        assert!(!w.is_running());
        assert!(!w.holds_sleep_delay());
        // Locked once, not once per failed reconnect attempt.
        std::thread::sleep(RECONNECT_FIRST + Duration::from_millis(500));
        assert_eq!(recorder.got().len(), 3, "{:?}", recorder.got());
        drop(w);
    }

    #[test]
    fn the_real_vault_and_cache_are_locked_by_a_sleep_signal() {
        use crate::lock_events::LockHooks;
        use crate::reference::SecretId;
        use crate::session_cache::{CacheLifetime, SessionSecretCache};
        use crate::vault::{KdfParams, Vault};
        use secrecy::SecretString;

        let Some(bus) = private_bus() else {
            eprintln!("dbus-daemon not available: skipped");
            return;
        };
        let logind = FakeLogind::start(&bus.address);
        let dir = tempfile::tempdir().unwrap();
        let vault = Arc::new(
            Vault::open_at(dir.path().join("vault.bin"), &dir.path().join("vault.lock")).unwrap(),
        );
        vault
            .create(
                &SecretString::from("master-pw-0123".to_owned()),
                KdfParams::insecure_for_tests(),
            )
            .unwrap();
        let cache = Arc::new(SessionSecretCache::new());
        cache.insert(
            SecretId::new("c:auth").unwrap(),
            SecretString::from("typed".to_owned()),
            CacheLifetime::Session,
        );
        let hooks = Arc::new(LockHooks::new());
        hooks.add(vault.clone());
        hooks.add(cache.clone());
        let _w = watcher(&bus, SessionChoice::Path(OUR_SESSION.into()), hooks);
        assert!(vault.is_unlocked());
        logind.emit(LOGIND_PATH, MANAGER, "PrepareForSleep", Some(true));
        wait("vault locked and cache cleared", || {
            !vault.is_unlocked() && cache.is_empty()
        });
    }

    #[test]
    fn an_unreachable_bus_is_an_error_not_a_hang() {
        let dir = tempfile::tempdir().unwrap();
        let address = format!("unix:path={}", dir.path().join("no-bus").display());
        let started = Instant::now();
        let err = DesktopLockWatcher::start_with(
            Target::Address(address),
            SessionChoice::Lookup,
            Arc::new(Recorder::default()),
            WATCHER_SETUP_TIMEOUT,
            Arc::new(suspended_since_boot),
        )
        .unwrap_err();
        assert!(matches!(err, LockWatchError::Bus(_)), "{err:?}");
        assert!(started.elapsed() <= WATCHER_SETUP_TIMEOUT + Duration::from_secs(1));
    }

    #[test]
    fn a_silent_bus_times_out_and_the_setup_thread_ends() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("silent-bus");
        let listener = UnixListener::bind(&path).unwrap();
        let (accepted_tx, accepted_rx) = mpsc::channel();
        std::thread::spawn(move || {
            if let Ok((s, _)) = listener.accept() {
                let _ = accepted_tx.send(s);
            }
        });
        let err = DesktopLockWatcher::start_with(
            Target::Address(format!("unix:path={}", path.display())),
            SessionChoice::Lookup,
            Arc::new(Recorder::default()),
            Duration::from_millis(300),
            Arc::new(suspended_since_boot),
        )
        .unwrap_err();
        assert!(matches!(err, LockWatchError::Timeout), "{err:?}");
        // The client end is closed once the setup thread gave up: read sees end of file.
        let mut server = accepted_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        server
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut buf = [0u8; 256];
        loop {
            match server.read(&mut buf) {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) => panic!("the setup thread still holds the connection: {e}"),
            }
        }
    }

    /// The bus goes away and comes back at the same address: the watcher locked once,
    /// reconnects, subscribes again (signals work), takes the inhibitor again and, since the
    /// session is locked by then, locks at once.
    #[test]
    fn a_restarted_bus_is_reconnected_resubscribed_and_a_locked_session_locks() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("bus");
        let Some(bus) = private_bus_at(Some(&socket)) else {
            eprintln!("dbus-daemon not available: skipped");
            return;
        };
        let logind = FakeLogind::start(&bus.address);
        let recorder = Arc::new(Recorder::default());
        // The address without the printed `guid=`: the restarted bus has another GUID (as the
        // system bus address the application uses has none).
        let w = DesktopLockWatcher::start_with(
            Target::Address(format!("unix:path={}", socket.display())),
            SessionChoice::Path(OUR_SESSION.into()),
            recorder.clone(),
            WATCHER_SETUP_TIMEOUT,
            Arc::new(suspended_since_boot),
        )
        .unwrap();
        assert!(w.is_running());
        assert!(w.holds_sleep_delay());

        drop(logind);
        drop(bus);
        assert_eq!(recorder.wait_for(1), [LockTrigger::Requested]);
        wait("reconnecting", || w.state() == WatcherState::Reconnecting);
        assert!(!w.holds_sleep_delay());

        let _ = std::fs::remove_file(&socket);
        let bus = private_bus_at(Some(&socket)).unwrap();
        let logind = FakeLogind::start_with(&bus.address, 0, Hint::Locked);
        wait("watching again", || w.is_running());
        assert!(w.watches_session_lock());
        assert_eq!(
            recorder.wait_for(2),
            [LockTrigger::Requested, LockTrigger::SessionLock],
            "LockedHint was true on reconnect"
        );
        wait("inhibitor taken again", || w.holds_sleep_delay());
        assert_eq!(logind.inhibit_calls(), 1);

        // Forged signals are still refused on the new connection (security review L6): sent to
        // it by name, a forged owner change, and broadcasts from a peer that is not logind.
        let name = w.unique_name();
        assert!(!name.is_empty());
        let forger = connect_to(&bus.address);
        let to = Some(name.as_str());
        zbus::block_on(async {
            forger
                .emit_signal(to, OUR_SESSION, SESSION, "Lock", &())
                .await
                .unwrap();
            forger
                .emit_signal(to, LOGIND_PATH, MANAGER, "PrepareForSleep", &(true,))
                .await
                .unwrap();
            forger
                .emit_signal(
                    to,
                    "/org/freedesktop/DBus",
                    BUS,
                    "NameOwnerChanged",
                    &(LOGIND, "", forger.unique_name().unwrap().as_str()),
                )
                .await
                .unwrap();
            forger
                .emit_signal(
                    None::<&str>,
                    "/org/freedesktop/DBus",
                    BUS,
                    "NameOwnerChanged",
                    &(LOGIND, "", forger.unique_name().unwrap().as_str()),
                )
                .await
                .unwrap();
        });
        emit_on(&forger, LOGIND_PATH, MANAGER, "PrepareForSleep", Some(true));
        emit_on(&forger, OUR_SESSION, SESSION, "Lock", None);
        std::thread::sleep(Duration::from_millis(400));
        assert_eq!(recorder.got().len(), 2, "{:?}", recorder.got());
        assert!(w.holds_sleep_delay());
        assert!(!logind.inhibitor_released(0));

        // Subscribed again: the new bus's signals reach the hook.
        logind.hint.store(Hint::Unlocked as u8, Ordering::SeqCst);
        logind.emit(OUR_SESSION, SESSION, "Lock", None);
        assert_eq!(recorder.wait_for(3).len(), 3);
        logind.emit(LOGIND_PATH, MANAGER, "PrepareForSleep", Some(true));
        assert_eq!(
            recorder.wait_for(4),
            [
                LockTrigger::Requested,
                LockTrigger::SessionLock,
                LockTrigger::SessionLock,
                LockTrigger::Sleep
            ]
        );
        drop(w);
        wait("released on drop", || logind.inhibitor_released(0));
    }

    #[test]
    fn a_lock_missed_while_logind_was_away_locks_when_it_returns() {
        let Some(bus) = private_bus() else {
            eprintln!("dbus-daemon not available: skipped");
            return;
        };
        let first = FakeLogind::start(&bus.address);
        let recorder = Arc::new(Recorder::default());
        let w = watcher(
            &bus,
            SessionChoice::Path(OUR_SESSION.into()),
            recorder.clone(),
        );
        zbus::block_on(first.conn.release_name(LOGIND)).unwrap();
        assert_eq!(recorder.wait_for(1), [LockTrigger::Requested]);
        wait("logind absent", || w.state() == WatcherState::LogindAbsent);
        let _second = FakeLogind::start_with(&bus.address, 0, Hint::Locked);
        assert_eq!(
            recorder.wait_for(2),
            [LockTrigger::Requested, LockTrigger::SessionLock]
        );
        wait("watching again", || w.is_running());
        drop(w);
    }

    /// A watcher on the bus at `socket`, with a settable suspended time.
    fn watcher_at(
        socket: &std::path::Path,
        session: SessionChoice,
        hook: Arc<dyn LockHook>,
        suspended: &Arc<StdMutex<Duration>>,
    ) -> DesktopLockWatcher {
        let s = Arc::clone(suspended);
        DesktopLockWatcher::start_with(
            Target::Address(format!("unix:path={}", socket.display())),
            session,
            hook,
            WATCHER_SETUP_TIMEOUT,
            Arc::new(move || *s.lock().unwrap()),
        )
        .unwrap()
    }

    /// Security review M1: the bus comes back before logind. The session found before stays
    /// watched (the redial's lookup fails), it is looked up again once logind is back, and its
    /// lock signals still work.
    #[test]
    fn the_session_stays_watched_when_logind_is_back_after_the_bus() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("bus");
        let Some(bus) = private_bus_at(Some(&socket)) else {
            eprintln!("dbus-daemon not available: skipped");
            return;
        };
        let logind = FakeLogind::start(&bus.address);
        let recorder = Arc::new(Recorder::default());
        let suspended = Arc::default();
        // Found through the user's display session.
        let w = watcher_at(&socket, SessionChoice::Lookup, recorder.clone(), &suspended);
        assert!(w.watches_session_lock());
        drop(logind);
        drop(bus);
        assert_eq!(recorder.wait_for(1), [LockTrigger::Requested]);

        // The bus is back, logind is not yet.
        let _ = std::fs::remove_file(&socket);
        let bus = private_bus_at(Some(&socket)).unwrap();
        wait("reconnected without logind", || {
            w.state() == WatcherState::LogindAbsent
        });
        assert!(w.watches_session_lock(), "the earlier session is kept");
        assert!(!w.session_lock_lost());
        assert!(!w.is_running());

        let logind = FakeLogind::start(&bus.address);
        wait("watching again", || w.is_running());
        wait("inhibitor taken", || w.holds_sleep_delay());
        // Let the lookup and the hint read finish (the hint says unlocked).
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(recorder.got(), [LockTrigger::Requested]);
        logind.emit(OUR_SESSION, SESSION, "Lock", None);
        assert_eq!(
            recorder.wait_for(2),
            [LockTrigger::Requested, LockTrigger::SessionLock]
        );
        assert!(w.watches_session_lock());
        drop(w);
    }

    /// Brings the bus at `socket` down and back up without logind, with a watcher that found
    /// its session through the user before: returns the new bus once the watcher reconnected.
    fn bus_back_before_logind(
        socket: &std::path::Path,
        bus: Bus,
        logind: FakeLogind,
        w: &DesktopLockWatcher,
        recorder: &Recorder,
    ) -> Bus {
        drop(logind);
        drop(bus);
        assert_eq!(recorder.wait_for(1), [LockTrigger::Requested]);
        let _ = std::fs::remove_file(socket);
        let bus = private_bus_at(Some(socket)).unwrap();
        wait("reconnected without logind", || {
            w.state() == WatcherState::LogindAbsent
        });
        bus
    }

    /// Re-review N1 / N2: logind is back but the session cannot be looked up again, retries
    /// included: the stale session is reported as possibly lost, and nothing is retried after
    /// the last attempt.
    #[test]
    fn a_session_not_found_again_after_retries_is_reported_lost() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("bus");
        let Some(bus) = private_bus_at(Some(&socket)) else {
            eprintln!("dbus-daemon not available: skipped");
            return;
        };
        let logind = FakeLogind::start(&bus.address);
        let recorder = Arc::new(Recorder::default());
        let suspended = Arc::default();
        let w = watcher_at(&socket, SessionChoice::Lookup, recorder.clone(), &suspended);
        let bus = bus_back_before_logind(&socket, bus, logind, &w, &recorder);
        assert!(!w.session_lock_lost(), "not known yet while logind is away");

        let logind = FakeLogind::start_full(&bus.address, 0, Hint::Unlocked, usize::MAX);
        wait("watching again", || w.is_running());
        wait("reported lost", || w.session_lock_lost());
        wait("first lookup and two retries", || {
            logind.user_lookups.load(Ordering::SeqCst) == 1 + RELOOKUP_RETRIES as usize
        });
        std::thread::sleep(RELOOKUP_RETRY_FIRST * 8);
        assert_eq!(
            logind.user_lookups.load(Ordering::SeqCst),
            1 + RELOOKUP_RETRIES as usize,
            "no retry after the last"
        );
        assert!(w.session_lock_lost());
        assert!(w.is_running(), "sleep is still watched");
        drop(w);
    }

    /// Re-review N2: a lookup that fails once succeeds on its retry, and the report clears.
    #[test]
    fn a_failed_session_lookup_is_retried_and_the_report_clears() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("bus");
        let Some(bus) = private_bus_at(Some(&socket)) else {
            eprintln!("dbus-daemon not available: skipped");
            return;
        };
        let logind = FakeLogind::start(&bus.address);
        let recorder = Arc::new(Recorder::default());
        let suspended = Arc::default();
        let w = watcher_at(&socket, SessionChoice::Lookup, recorder.clone(), &suspended);
        let bus = bus_back_before_logind(&socket, bus, logind, &w, &recorder);

        let logind = FakeLogind::start_full(&bus.address, 0, Hint::Unlocked, 1);
        wait("retried", || {
            logind.user_lookups.load(Ordering::SeqCst) == 2
        });
        wait("report cleared", || !w.session_lock_lost());
        assert!(w.watches_session_lock());
        std::thread::sleep(Duration::from_millis(200));
        logind.emit(OUR_SESSION, SESSION, "Lock", None);
        assert_eq!(
            recorder.wait_for(2),
            [LockTrigger::Requested, LockTrigger::SessionLock]
        );
        drop(w);
    }

    /// Re-review N3: waits fire in deadline order, not in the order they were asked for, and
    /// the thread ends when the timer is dropped.
    #[test]
    fn the_timer_thread_fires_in_order_and_ends_on_drop() {
        let mut timer = TimerThread::start().unwrap();
        let fired: Arc<StdMutex<Vec<usize>>> = Arc::default();
        let started = Instant::now();
        let waiters: Vec<_> = [300_u64, 100, 200, 0]
            .into_iter()
            .enumerate()
            .map(|(i, ms)| {
                let wait = timer.sleep(Duration::from_millis(ms));
                let fired = Arc::clone(&fired);
                std::thread::spawn(move || {
                    zbus::block_on(wait);
                    let elapsed = started.elapsed();
                    assert!(elapsed >= Duration::from_millis(ms), "{i}: {elapsed:?}");
                    fired.lock().unwrap().push(i);
                })
            })
            .collect();
        for w in waiters {
            w.join().unwrap();
        }
        assert_eq!(*fired.lock().unwrap(), [3, 1, 2, 0]);

        // A pending wait does not keep the thread alive.
        let _pending = timer.sleep(Duration::from_secs(3600));
        let handle = timer.handle.take().unwrap();
        drop(timer);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !handle.is_finished() {
            assert!(Instant::now() < deadline, "the timer thread did not end");
            std::thread::sleep(Duration::from_millis(10));
        }
        handle.join().unwrap();
    }

    /// Security review M1: no session could be found while logind was absent; it is looked up
    /// and subscribed to once logind appears.
    #[test]
    fn a_session_unknown_without_logind_is_looked_up_when_it_appears() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("bus");
        let Some(bus) = private_bus_at(Some(&socket)) else {
            eprintln!("dbus-daemon not available: skipped");
            return;
        };
        let recorder = Arc::new(Recorder::default());
        let suspended = Arc::default();
        let w = watcher_at(&socket, SessionChoice::Lookup, recorder.clone(), &suspended);
        assert_eq!(w.state(), WatcherState::LogindAbsent);
        assert!(!w.watches_session_lock());
        assert!(!w.session_lock_lost(), "never had one");

        let logind = FakeLogind::start(&bus.address);
        wait("watching", || w.is_running());
        wait("session found", || w.watches_session_lock());
        std::thread::sleep(Duration::from_millis(200));
        logind.emit(OUR_SESSION, SESSION, "Lock", None);
        assert_eq!(recorder.wait_for(1), [LockTrigger::SessionLock]);
        drop(w);
    }

    /// Security review L2: a suspend while logind was away locks when it returns; clock drift
    /// under the slack does not.
    #[test]
    fn a_suspend_while_logind_was_away_locks_when_it_returns() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("bus");
        let Some(_bus) = private_bus_at(Some(&socket)) else {
            eprintln!("dbus-daemon not available: skipped");
            return;
        };
        let address = format!("unix:path={}", socket.display());
        let first = FakeLogind::start(&address);
        let recorder = Arc::new(Recorder::default());
        let suspended: Arc<StdMutex<Duration>> = Arc::default();
        let w = watcher_at(
            &socket,
            SessionChoice::Path(OUR_SESSION.into()),
            recorder.clone(),
            &suspended,
        );

        zbus::block_on(first.conn.release_name(LOGIND)).unwrap();
        wait("logind absent", || w.state() == WatcherState::LogindAbsent);
        *suspended.lock().unwrap() += Duration::from_secs(1);
        let second = FakeLogind::start(&address);
        wait("watching again", || w.is_running());
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(recorder.got(), [LockTrigger::Requested]);

        zbus::block_on(second.conn.release_name(LOGIND)).unwrap();
        wait("logind absent", || w.state() == WatcherState::LogindAbsent);
        *suspended.lock().unwrap() += Duration::from_secs(30);
        let _third = FakeLogind::start(&address);
        assert_eq!(
            recorder.wait_for(3),
            [
                LockTrigger::Requested,
                LockTrigger::Requested,
                LockTrigger::Sleep
            ]
        );
        drop(w);
    }

    /// Security review L3: after an outage, a `LockedHint` that cannot be read locks.
    #[test]
    fn an_unreadable_locked_hint_after_an_outage_locks() {
        let Some(bus) = private_bus() else {
            eprintln!("dbus-daemon not available: skipped");
            return;
        };
        let first = FakeLogind::start(&bus.address);
        let recorder = Arc::new(Recorder::default());
        let w = watcher(
            &bus,
            SessionChoice::Path(OUR_SESSION.into()),
            recorder.clone(),
        );
        zbus::block_on(first.conn.release_name(LOGIND)).unwrap();
        assert_eq!(recorder.wait_for(1), [LockTrigger::Requested]);
        let _second = FakeLogind::start_with(&bus.address, 0, Hint::Fails);
        assert_eq!(
            recorder.wait_for(2),
            [LockTrigger::Requested, LockTrigger::SessionLock]
        );
        drop(w);
    }

    // ---- Reconnect loop without a bus: fake dialer, paused tokio time. ----

    struct TokioClocks {
        origin: tokio::time::Instant,
        suspended: Arc<StdMutex<Duration>>,
    }

    impl Clocks for TokioClocks {
        fn sleep(&self, d: Duration) -> BoxFuture<'static, ()> {
            Box::pin(tokio::time::sleep(d))
        }

        fn now(&self) -> Duration {
            self.origin.elapsed()
        }

        fn suspended(&self) -> Duration {
            *self.suspended.lock().unwrap()
        }
    }

    /// The fake system bus: up or down, or never answering.
    #[derive(Default)]
    struct Net {
        up: AtomicBool,
        hang: AtomicBool,
        /// When each dial was attempted (time since the harness started).
        dials: StdMutex<Vec<Duration>>,
        /// `after_outage` of each link run.
        runs: StdMutex<Vec<bool>>,
        /// Ends the running link (the connection is lost).
        lose: tokio::sync::Notify,
    }

    struct FakeDialer {
        net: Arc<Net>,
        origin: tokio::time::Instant,
    }

    impl Dialer for FakeDialer {
        type Link = FakeLink;

        fn dial(&mut self) -> BoxFuture<'_, Result<FakeLink, LockWatchError>> {
            self.net.dials.lock().unwrap().push(self.origin.elapsed());
            let net = Arc::clone(&self.net);
            Box::pin(async move {
                if net.hang.load(Ordering::SeqCst) {
                    std::future::pending::<()>().await;
                }
                if net.up.load(Ordering::SeqCst) {
                    Ok(FakeLink { net })
                } else {
                    Err(LockWatchError::Bus("no bus".into()))
                }
            })
        }
    }

    struct FakeLink {
        net: Arc<Net>,
    }

    impl Link for FakeLink {
        fn publish(&self, status: &Status) {
            status.set_state(WatcherState::Watching);
            status.sleep_delay.store(true, Ordering::SeqCst);
            status.session_watched.store(true, Ordering::SeqCst);
        }

        fn run(
            self,
            _hook: Arc<dyn LockHook>,
            _status: Arc<Status>,
            _clocks: Arc<dyn Clocks>,
            after_outage: bool,
        ) -> BoxFuture<'static, ()> {
            self.net.runs.lock().unwrap().push(after_outage);
            Box::pin(async move { self.net.lose.notified().await })
        }
    }

    struct Harness {
        net: Arc<Net>,
        stop: Arc<Stop>,
        status: Arc<Status>,
        recorder: Arc<Recorder>,
        suspended: Arc<StdMutex<Duration>>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Harness {
        /// A watcher whose first connection is up.
        async fn start() -> Self {
            let origin = tokio::time::Instant::now();
            let net = Arc::new(Net::default());
            net.up.store(true, Ordering::SeqCst);
            let stop = Arc::new(Stop::default());
            let status = Arc::new(Status::default());
            let recorder = Arc::new(Recorder::default());
            let suspended: Arc<StdMutex<Duration>> = Arc::default();
            let first = FakeLink {
                net: Arc::clone(&net),
            };
            let dialer = FakeDialer {
                net: Arc::clone(&net),
                origin,
            };
            let clocks: Arc<dyn Clocks> = Arc::new(TokioClocks {
                origin,
                suspended: Arc::clone(&suspended),
            });
            let (s, st, hook) = (
                Arc::clone(&stop),
                Arc::clone(&status),
                Arc::clone(&recorder) as Arc<dyn LockHook>,
            );
            let task = tokio::spawn(async move {
                supervise(dialer, first, hook, &s, &st, clocks).await;
            });
            let h = Self {
                net,
                stop,
                status,
                recorder,
                suspended,
                task,
            };
            settle().await;
            assert_eq!(h.status.state(), WatcherState::Watching);
            h
        }

        /// The connection drops; the bus is `up` afterwards or not.
        async fn lose(&self, up: bool) {
            self.net.up.store(up, Ordering::SeqCst);
            self.net.lose.notify_one();
            settle().await;
        }

        fn dial_secs(&self) -> Vec<u64> {
            self.net
                .dials
                .lock()
                .unwrap()
                .iter()
                .map(Duration::as_secs)
                .collect()
        }
    }

    /// Lets the supervisor run without moving the paused clock.
    async fn settle() {
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
    }

    async fn sleep_secs(s: u64) {
        tokio::time::sleep(Duration::from_secs(s)).await;
        settle().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_lost_bus_locks_once_backs_off_within_bounds_and_recovery_clears_the_problem() {
        let h = Harness::start().await;
        assert_eq!(*h.net.runs.lock().unwrap(), [false]);
        h.lose(false).await;
        assert_eq!(h.recorder.got(), [LockTrigger::Requested]);
        assert_eq!(h.status.state(), WatcherState::Reconnecting);
        assert!(!h.status.sleep_delay.load(Ordering::SeqCst));

        sleep_secs(600).await;
        // 1 s doubling to 60 s, never faster, never slower.
        assert_eq!(
            h.dial_secs(),
            [1, 3, 7, 15, 31, 63, 123, 183, 243, 303, 363, 423, 483, 543]
        );
        assert_eq!(h.recorder.got(), [LockTrigger::Requested], "locked once");
        assert_eq!(h.status.state(), WatcherState::Reconnecting);

        // The bus is back: the next attempt (at 603 s) subscribes again and the state clears.
        h.net.up.store(true, Ordering::SeqCst);
        sleep_secs(3).await;
        assert_eq!(*h.dial_secs().last().unwrap(), 603);
        assert_eq!(h.status.state(), WatcherState::Watching);
        assert!(h.status.sleep_delay.load(Ordering::SeqCst));
        assert_eq!(*h.net.runs.lock().unwrap(), [false, true]);
        assert_eq!(h.recorder.got(), [LockTrigger::Requested]);
        sleep_secs(600).await;
        assert_eq!(h.dial_secs().len(), 15, "no attempt while connected");
        h.stop.stop();
    }

    #[tokio::test(start_paused = true)]
    async fn a_reconnect_that_hangs_times_out_and_is_retried() {
        let h = Harness::start().await;
        h.net.hang.store(true, Ordering::SeqCst);
        h.lose(false).await;
        sleep_secs(100).await;
        // Each attempt is cut after WATCHER_SETUP_TIMEOUT (5 s), then the back-off applies.
        assert_eq!(h.dial_secs(), [1, 8, 17, 30, 51, 88]);
        assert_eq!(h.status.state(), WatcherState::Reconnecting);
        h.net.hang.store(false, Ordering::SeqCst);
        h.net.up.store(true, Ordering::SeqCst);
        sleep_secs(60).await;
        assert_eq!(h.dial_secs(), [1, 8, 17, 30, 51, 88, 153]);
        assert_eq!(h.status.state(), WatcherState::Watching);
        assert_eq!(h.recorder.got(), [LockTrigger::Requested]);
        h.stop.stop();
    }

    #[tokio::test(start_paused = true)]
    async fn a_flapping_bus_keeps_the_back_off_and_a_stable_one_resets_it() {
        let h = Harness::start().await;
        h.lose(true).await;
        sleep_secs(1).await;
        assert_eq!(h.status.state(), WatcherState::Watching);
        // Lost again at once: the next wait is 2 s, then 4 s, not 1 s every time.
        h.lose(true).await;
        sleep_secs(2).await;
        h.lose(true).await;
        sleep_secs(4).await;
        assert_eq!(h.dial_secs(), [1, 3, 7]);
        // Up for a minute: the back-off starts again at 1 s.
        sleep_secs(60).await;
        h.lose(true).await;
        sleep_secs(1).await;
        assert_eq!(h.dial_secs(), [1, 3, 7, 68]);
        assert_eq!(h.status.state(), WatcherState::Watching);
        // One lock per loss.
        assert_eq!(h.recorder.got(), [LockTrigger::Requested; 4]);
        h.stop.stop();
    }

    #[tokio::test(start_paused = true)]
    async fn a_sleep_missed_during_the_outage_locks_on_reconnect() {
        let h = Harness::start().await;
        // A second of drift between the clocks is not a suspend.
        h.lose(true).await;
        *h.suspended.lock().unwrap() += Duration::from_secs(1);
        sleep_secs(1).await;
        assert_eq!(h.status.state(), WatcherState::Watching);
        assert_eq!(h.recorder.got(), [LockTrigger::Requested]);
        // The machine slept 30 s while the bus was away.
        h.lose(false).await;
        *h.suspended.lock().unwrap() += Duration::from_secs(30);
        h.net.up.store(true, Ordering::SeqCst);
        sleep_secs(2).await;
        assert_eq!(h.status.state(), WatcherState::Watching);
        assert_eq!(
            h.recorder.got(),
            [
                LockTrigger::Requested,
                LockTrigger::Requested,
                LockTrigger::Sleep
            ]
        );
        h.stop.stop();
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_stops_the_retry_loop() {
        // During a back-off wait.
        let h = Harness::start().await;
        h.lose(false).await;
        sleep_secs(10).await;
        let attempts = h.dial_secs().len();
        h.stop.stop();
        settle().await;
        assert!(h.task.is_finished());
        sleep_secs(600).await;
        assert_eq!(h.dial_secs().len(), attempts);

        // During a reconnect attempt that never answers.
        let h = Harness::start().await;
        h.net.hang.store(true, Ordering::SeqCst);
        h.lose(false).await;
        sleep_secs(1).await;
        assert_eq!(h.dial_secs(), [1]);
        h.stop.stop();
        settle().await;
        assert!(h.task.is_finished());

        // While connected.
        let h = Harness::start().await;
        h.stop.stop();
        settle().await;
        assert!(h.task.is_finished());
        assert!(h.recorder.got().is_empty(), "a stop is not a loss");
    }

    #[test]
    fn the_state_round_trips_and_the_guard_clears_it() {
        for s in [
            WatcherState::Watching,
            WatcherState::LogindAbsent,
            WatcherState::Reconnecting,
            WatcherState::Stopped,
        ] {
            assert_eq!(WatcherState::from_u8(s.to_u8()), s);
        }
        let status = Arc::new(Status::default());
        assert_eq!(status.state(), WatcherState::Stopped);
        status.set_state(WatcherState::Watching);
        status.sleep_delay.store(true, Ordering::SeqCst);
        drop(RunningGuard(Arc::clone(&status)));
        assert_eq!(status.state(), WatcherState::Stopped);
        assert!(!status.sleep_delay.load(Ordering::SeqCst));
    }

    #[test]
    fn the_suspended_time_does_not_go_backwards() {
        let a = suspended_since_boot();
        let b = suspended_since_boot();
        // Two non-atomic reads may differ by a little, never by a suspend.
        assert!(b + MISSED_SLEEP_SLACK > a, "{a:?} {b:?}");
    }
}
