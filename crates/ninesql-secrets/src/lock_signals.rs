//! logind listener for [`crate::lock_events`] (feature `secret-service`, same zbus 6 as the
//! Secret Service backend: no new dependency).
//!
//! [`DesktopLockWatcher::start`] opens its own system-bus connection on a thread of its own and
//! calls a [`LockHook`] on:
//! - `org.freedesktop.login1.Manager.PrepareForSleep(true)` (suspend or hibernate about to
//!   start). The watcher holds a logind **delay inhibitor** for sleep (`Inhibit("sleep", …,
//!   "delay")`), so logind waits (up to its `InhibitDelayMaxSec`) until the hook has run; the
//!   inhibitor is released right after the hook, and taken again on `PrepareForSleep(false)`;
//! - the screen lock of the session this process belongs to: the session's `Lock` signal, and
//!   its `LockedHint` property turning `true` (`PropertiesChanged`; GNOME and KDE lockers set the
//!   hint rather than emit `Lock`). The session is found with `GetSessionByPID`, else
//!   `$XDG_SESSION_ID`, else the user's display session (`GetUser(uid)` → `User.Display`, for
//!   applications started in an `app-*.scope` outside the session). Without a session, only sleep
//!   is watched ([`DesktopLockWatcher::watches_session_lock`]).
//!
//! Every subscription is restricted to signals sent by the owner of `org.freedesktop.login1`
//! (the bus checks the sender); member, interface and path are checked again on receipt.
//! On the bus the watcher only calls logind: the session lookup and `Inhibit`.
//!
//! Fail closed: a panicking hook is logged and the watcher goes on; when the bus connection is
//! lost, the hook is called once with [`LockTrigger::Requested`] and the watcher stops
//! ([`DesktopLockWatcher::is_running`]). Dropping the watcher stops its thread, during setup too.

use std::collections::HashMap;
use std::future::{Future, poll_fn};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::{Pin, pin};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

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

/// State shared with the watcher thread.
#[derive(Default)]
struct Status {
    running: AtomicBool,
    sleep_delay: AtomicBool,
}

/// Clears the status when the watcher thread ends, panic included.
struct RunningGuard(Arc<Status>);

impl Drop for RunningGuard {
    fn drop(&mut self) {
        self.0.running.store(false, Ordering::SeqCst);
        self.0.sleep_delay.store(false, Ordering::SeqCst);
    }
}

/// Listens to logind's sleep and session-lock signals until dropped.
pub struct DesktopLockWatcher {
    stop: Arc<Stop>,
    status: Arc<Status>,
    session_watched: bool,
}

impl std::fmt::Debug for DesktopLockWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DesktopLockWatcher")
            .field("session_watched", &self.session_watched)
            .field("running", &self.is_running())
            .finish_non_exhaustive()
    }
}

impl DesktopLockWatcher {
    /// Starts watching the system bus. Blocks for at most [`WATCHER_SETUP_TIMEOUT`] (call it on
    /// a background thread, never on the UI thread).
    ///
    /// # Errors
    /// [`LockWatchError`] when there is no system bus or it does not answer; the application
    /// then relies on the idle timers and its own lock action.
    pub fn start(hook: Arc<dyn LockHook>) -> Result<Self, LockWatchError> {
        Self::start_with(
            Target::System,
            SessionChoice::Lookup,
            hook,
            WATCHER_SETUP_TIMEOUT,
        )
    }

    /// Whether the current session's screen lock is watched (`false`: no logind session was
    /// found for this process; only sleep is watched).
    #[must_use]
    pub fn watches_session_lock(&self) -> bool {
        self.session_watched
    }

    /// Whether the watcher still listens. `false` once the bus connection was lost (the hook
    /// was then called with [`LockTrigger::Requested`]); the application may start a new one.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.status.running.load(Ordering::SeqCst)
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
                    target,
                    session,
                    hook,
                    thread_stop,
                    thread_status,
                    ready_tx,
                ));
            })
            .map_err(|e| LockWatchError::Bus(format!("cannot start the watcher thread: {e}")))?;
        match ready_rx.recv_timeout(setup_timeout) {
            Ok(Ok(session_watched)) => Ok(Self {
                stop,
                status,
                session_watched,
            }),
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
        .inspect_err(|e| tracing::info!(error = %e, "no sleep delay inhibitor"))
        .ok()?;
    reply.body().deserialize::<OwnedFd>().ok()
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Event {
    SleepStarting,
    Resumed,
    SessionLocked,
}

/// What a received signal asks for (second check after the bus-side match rules).
fn classify(msg: &Message, session_path: Option<&str>) -> Option<Event> {
    let header = msg.header();
    if header.message_type() != MessageType::Signal {
        return None;
    }
    let interface = header.interface()?.as_str();
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

async fn watch(
    target: Target,
    session: SessionChoice,
    hook: Arc<dyn LockHook>,
    stop: Arc<Stop>,
    status: Arc<Status>,
    ready: mpsc::Sender<Result<bool, LockWatchError>>,
) {
    let setup = async {
        let conn = connect(target).await?;
        let session_path = match session {
            SessionChoice::Lookup => own_session(&conn).await,
            #[cfg(test)]
            SessionChoice::Path(p) => Some(p),
        };
        let mut streams =
            vec![subscribe(&conn, signal_rule(MANAGER, "PrepareForSleep", LOGIND_PATH)).await?];
        if let Some(p) = &session_path {
            streams.push(subscribe(&conn, signal_rule(SESSION, "Lock", p)).await?);
            let props = MatchRule::builder()
                .msg_type(MessageType::Signal)
                .sender(LOGIND)
                .interface(PROPERTIES)
                .member("PropertiesChanged")
                .path(p.clone())
                .arg(0, SESSION)
                .build()
                .map(|r| r.to_owned());
            streams.push(subscribe(&conn, props).await?);
        }
        let inhibitor = take_sleep_delay(&conn).await;
        Ok::<_, LockWatchError>((conn, session_path, streams, inhibitor))
    };
    let (conn, session_path, mut streams, mut inhibitor) = match until_stopped(&stop, setup).await {
        None => return,
        Some(Ok(v)) => v,
        Some(Err(e)) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    if session_path.is_none() {
        tracing::info!("no logind session found: only sleep locks the secrets");
    }
    status.running.store(true, Ordering::SeqCst);
    status
        .sleep_delay
        .store(inhibitor.is_some(), Ordering::SeqCst);
    if ready.send(Ok(session_path.is_some())).is_err() {
        // `start` gave up waiting.
        return;
    }
    loop {
        let next = poll_fn(|cx| {
            if stop.poll_stopped(cx) {
                return Poll::Ready(None);
            }
            for s in &mut streams {
                if let Poll::Ready(m) = Pin::new(s).poll_next(cx) {
                    return Poll::Ready(Some(m));
                }
            }
            Poll::Pending
        })
        .await;
        match next {
            None => break,
            Some(Some(Ok(msg))) => match classify(&msg, session_path.as_deref()) {
                Some(Event::SleepStarting) => {
                    run_hook(&*hook, LockTrigger::Sleep);
                    // Secrets are gone: let the suspend go on.
                    inhibitor = None;
                    status.sleep_delay.store(false, Ordering::SeqCst);
                }
                Some(Event::Resumed) => {
                    if inhibitor.is_none() {
                        inhibitor = until_stopped(&stop, take_sleep_delay(&conn))
                            .await
                            .flatten();
                        status
                            .sleep_delay
                            .store(inhibitor.is_some(), Ordering::SeqCst);
                    }
                }
                Some(Event::SessionLocked) => run_hook(&*hook, LockTrigger::SessionLock),
                None => {}
            },
            Some(Some(Err(e))) => tracing::debug!(error = %e, "bad message on the system bus"),
            Some(None) => {
                tracing::warn!(
                    "system bus connection lost: locking secrets; sleep and lock no longer watched"
                );
                run_hook(&*hook, LockTrigger::Requested);
                break;
            }
        }
    }
    drop(inhibitor);
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

    /// `None` when `dbus-daemon` is not installed: the bus tests are skipped.
    fn private_bus() -> Option<Bus> {
        let mut child = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
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
    }

    const OUR_SESSION: &str = "/org/freedesktop/login1/session/_31";
    const OTHER_SESSION: &str = "/org/freedesktop/login1/session/_32";
    const USER_PATH: &str = "/org/freedesktop/login1/user/_1000";

    impl FakeLogind {
        fn start(address: &str) -> Self {
            let conn = connect_to(address);
            zbus::block_on(conn.request_name(LOGIND)).unwrap();
            let inhibitors: Arc<StdMutex<Vec<UnixStream>>> = Arc::default();
            let (c, inh) = (conn.clone(), Arc::clone(&inhibitors));
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
                            Some("GetUser") => {
                                let p = OwnedObjectPath::try_from(USER_PATH).unwrap();
                                c.reply(&h, &p).await.unwrap();
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
            Self { conn, inhibitors }
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
        wait("watcher stopped", || !w.is_running());
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
                &SecretString::from("master-pw".to_owned()),
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
}
