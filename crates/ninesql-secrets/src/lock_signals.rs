//! logind listener for [`crate::lock_events`] (feature `secret-service`, same zbus 6 as the
//! Secret Service backend: no new dependency).
//!
//! [`DesktopLockWatcher::start`] opens its own system-bus connection on a thread of its own and
//! calls a [`LockHook`] on:
//! - `org.freedesktop.login1.Manager.PrepareForSleep(true)` (suspend or hibernate about to
//!   start; `false`, the resume, is ignored);
//! - `org.freedesktop.login1.Session.Lock` of the session this process belongs to (found with
//!   `GetSessionByPID`, else `$XDG_SESSION_ID`). Without a session, only sleep is watched.
//!
//! The sender of a signal is not checked: a forged signal can only lock secrets (fail-safe), never
//! unlock them or read anything. Nothing is sent on the bus except the session lookup at start.
//! Dropping the watcher stops its thread.

use std::future::poll_fn;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::task::{Poll, Waker};
use std::time::Duration;

use zbus::export::futures_core::Stream;
use zbus::message::Type as MessageType;
use zbus::{Connection, MatchRule, Message, MessageStream, OwnedObjectPath};

use crate::lock_events::{LockHook, LockTrigger};

const LOGIND: &str = "org.freedesktop.login1";
const LOGIND_PATH: &str = "/org/freedesktop/login1";
const MANAGER: &str = "org.freedesktop.login1.Manager";
const SESSION: &str = "org.freedesktop.login1.Session";

/// Longest wait for the bus connection, the session lookup and the subscriptions.
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

/// Which session's `Lock` signal to watch.
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
}

/// Listens to logind's sleep and session-lock signals until dropped.
pub struct DesktopLockWatcher {
    stop: Arc<Stop>,
    session_watched: bool,
}

impl std::fmt::Debug for DesktopLockWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DesktopLockWatcher")
            .field("session_watched", &self.session_watched)
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
        Self::start_with(Target::System, SessionChoice::Lookup, hook)
    }

    /// Whether the current session's `Lock` signal is watched (`false`: no logind session was
    /// found for this process; only sleep is watched).
    #[must_use]
    pub fn watches_session_lock(&self) -> bool {
        self.session_watched
    }

    fn start_with(
        target: Target,
        session: SessionChoice,
        hook: Arc<dyn LockHook>,
    ) -> Result<Self, LockWatchError> {
        let stop = Arc::new(Stop::default());
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread_stop = Arc::clone(&stop);
        std::thread::Builder::new()
            .name("ninesql-lock-signals".into())
            .spawn(move || {
                zbus::block_on(watch(target, session, hook, thread_stop, ready_tx));
            })
            .map_err(|e| LockWatchError::Bus(format!("cannot start the watcher thread: {e}")))?;
        match ready_rx.recv_timeout(WATCHER_SETUP_TIMEOUT) {
            Ok(Ok(session_watched)) => Ok(Self {
                stop,
                session_watched,
            }),
            Ok(Err(e)) => Err(e),
            Err(mpsc::RecvTimeoutError::Timeout) => {
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

/// The object path of this process's logind session, if any.
async fn own_session(conn: &Connection) -> Option<String> {
    let by_pid = conn
        .call_method(
            Some(LOGIND),
            LOGIND_PATH,
            Some(MANAGER),
            "GetSessionByPID",
            &(std::process::id(),),
        )
        .await;
    let reply = match by_pid {
        Ok(m) => Some(m),
        Err(e) => {
            tracing::debug!(error = %e, "no logind session for this process");
            match std::env::var("XDG_SESSION_ID") {
                Ok(id) if !id.is_empty() => conn
                    .call_method(
                        Some(LOGIND),
                        LOGIND_PATH,
                        Some(MANAGER),
                        "GetSession",
                        &(id.as_str(),),
                    )
                    .await
                    .inspect_err(|e| tracing::debug!(error = %e, "XDG_SESSION_ID not known"))
                    .ok(),
                _ => None,
            }
        }
    };
    let path: OwnedObjectPath = reply?.body().deserialize().ok()?;
    Some(path.as_str().to_owned())
}

async fn subscribe(
    conn: &Connection,
    interface: &'static str,
    member: &'static str,
    path: &str,
) -> Result<MessageStream, LockWatchError> {
    let rule = MatchRule::builder()
        .msg_type(MessageType::Signal)
        .interface(interface)
        .member(member)
        .path(path)
        .build()
        .map_err(bus_error)?
        .to_owned();
    MessageStream::for_match_rule(rule, conn, None)
        .await
        .map_err(bus_error)
}

/// What a received signal asks for.
fn classify(msg: &Message, session_path: Option<&str>) -> Option<LockTrigger> {
    let header = msg.header();
    if header.message_type() != MessageType::Signal {
        return None;
    }
    let interface = header.interface()?.as_str();
    let member = header.member()?.as_str();
    let path = header.path()?.as_str();
    match (interface, member) {
        (MANAGER, "PrepareForSleep") if path == LOGIND_PATH => msg
            .body()
            .deserialize::<bool>()
            .ok()
            .filter(|starting| *starting)
            .map(|_| LockTrigger::Sleep),
        (SESSION, "Lock") if Some(path) == session_path => Some(LockTrigger::SessionLock),
        _ => None,
    }
}

async fn watch(
    target: Target,
    session: SessionChoice,
    hook: Arc<dyn LockHook>,
    stop: Arc<Stop>,
    ready: mpsc::Sender<Result<bool, LockWatchError>>,
) {
    let setup = async {
        let conn = connect(target).await?;
        let session_path = match session {
            SessionChoice::Lookup => own_session(&conn).await,
            #[cfg(test)]
            SessionChoice::Path(p) => Some(p),
        };
        let sleep = subscribe(&conn, MANAGER, "PrepareForSleep", LOGIND_PATH).await?;
        let lock = match &session_path {
            Some(p) => Some(subscribe(&conn, SESSION, "Lock", p).await?),
            None => None,
        };
        Ok::<_, LockWatchError>((conn, session_path, sleep, lock))
    };
    let (_conn, session_path, mut sleep, mut lock) = match setup.await {
        Ok(v) => v,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    if session_path.is_none() {
        tracing::info!("no logind session found: only sleep locks the secrets");
    }
    if ready.send(Ok(session_path.is_some())).is_err() {
        // `start` gave up waiting.
        return;
    }
    loop {
        let next = poll_fn(|cx| {
            if stop.stopped.load(Ordering::SeqCst) {
                return Poll::Ready(None);
            }
            *stop.waker.lock().unwrap_or_else(PoisonError::into_inner) = Some(cx.waker().clone());
            // Checked again: a stop between the first check and storing the waker is not lost.
            if stop.stopped.load(Ordering::SeqCst) {
                return Poll::Ready(None);
            }
            if let Poll::Ready(m) = Pin::new(&mut sleep).poll_next(cx) {
                return Poll::Ready(Some(m));
            }
            if let Some(lock) = lock.as_mut()
                && let Poll::Ready(m) = Pin::new(lock).poll_next(cx)
            {
                return Poll::Ready(Some(m));
            }
            Poll::Pending
        })
        .await;
        match next {
            None => break,
            Some(Some(Ok(msg))) => {
                if let Some(trigger) = classify(&msg, session_path.as_deref()) {
                    hook.lock_now(trigger);
                }
            }
            Some(Some(Err(e))) => tracing::debug!(error = %e, "bad message on the system bus"),
            Some(None) => {
                tracing::warn!("system bus connection closed: sleep and lock no longer watched");
                break;
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stderr)] // Tests may panic (AGENTS.md).
mod tests {
    use std::io::{BufRead, BufReader};
    use std::process::{Child, Command, Stdio};
    use std::sync::Mutex as StdMutex;
    use std::time::Instant;

    use super::*;

    /// Records the triggers it receives.
    #[derive(Default)]
    struct Recorder(StdMutex<Vec<LockTrigger>>);

    impl LockHook for Recorder {
        fn lock_now(&self, trigger: LockTrigger) {
            self.0.lock().unwrap().push(trigger);
        }
    }

    impl Recorder {
        fn wait_for(&self, n: usize) -> Vec<LockTrigger> {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let got = self.0.lock().unwrap().clone();
                if got.len() >= n || Instant::now() > deadline {
                    return got;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
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

    fn emit(address: &str, path: &str, interface: &str, member: &str, body: Option<bool>) {
        zbus::block_on(async {
            let conn = zbus::connection::Builder::address(address)
                .build()
                .await
                .unwrap();
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

    const OUR_SESSION: &str = "/org/freedesktop/login1/session/_31";
    const OTHER_SESSION: &str = "/org/freedesktop/login1/session/_32";

    #[test]
    fn sleep_and_own_session_lock_trigger_the_hook() {
        let Some(bus) = private_bus() else {
            eprintln!("dbus-daemon not available: skipped");
            return;
        };
        let recorder = Arc::new(Recorder::default());
        let watcher = DesktopLockWatcher::start_with(
            Target::Address(bus.address.clone()),
            SessionChoice::Path(OUR_SESSION.into()),
            recorder.clone(),
        )
        .unwrap();
        assert!(watcher.watches_session_lock());

        // Ignored: the resume, another session's lock, other members, wrong paths.
        emit(
            &bus.address,
            LOGIND_PATH,
            MANAGER,
            "PrepareForSleep",
            Some(false),
        );
        emit(&bus.address, OTHER_SESSION, SESSION, "Lock", None);
        emit(&bus.address, OUR_SESSION, SESSION, "Unlock", None);
        emit(
            &bus.address,
            "/elsewhere",
            MANAGER,
            "PrepareForSleep",
            Some(true),
        );
        // Acted on.
        emit(
            &bus.address,
            LOGIND_PATH,
            MANAGER,
            "PrepareForSleep",
            Some(true),
        );
        emit(&bus.address, OUR_SESSION, SESSION, "Lock", None);

        let got = recorder.wait_for(2);
        assert_eq!(got, [LockTrigger::Sleep, LockTrigger::SessionLock]);
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(
            recorder.0.lock().unwrap().len(),
            2,
            "nothing else triggered"
        );

        // Once dropped, nothing reaches the hook.
        drop(watcher);
        std::thread::sleep(Duration::from_millis(100));
        emit(
            &bus.address,
            LOGIND_PATH,
            MANAGER,
            "PrepareForSleep",
            Some(true),
        );
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(recorder.0.lock().unwrap().len(), 2);
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
        let _watcher = DesktopLockWatcher::start_with(
            Target::Address(bus.address.clone()),
            SessionChoice::Path(OUR_SESSION.into()),
            hooks,
        )
        .unwrap();
        assert!(vault.is_unlocked());
        emit(
            &bus.address,
            LOGIND_PATH,
            MANAGER,
            "PrepareForSleep",
            Some(true),
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        while vault.is_unlocked() || !cache.is_empty() {
            assert!(
                Instant::now() < deadline,
                "not locked after the sleep signal"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
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
        )
        .unwrap_err();
        assert!(matches!(err, LockWatchError::Bus(_)), "{err:?}");
        assert!(started.elapsed() <= WATCHER_SETUP_TIMEOUT + Duration::from_secs(1));
    }
}
