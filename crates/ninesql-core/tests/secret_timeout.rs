//! Wave 2 integration review M1: secret resolution in the connect flow is bounded by
//! `ManagerConfig::secret_timeout`, a panicking store is `SecretError::Unavailable`, and an open
//! whose `Reply` is dropped stops at once (no connect once the store answers).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // Tests may panic (AGENTS.md).

mod support;

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use ninesql_core::{
    Auth, ConnectionTypeId, ConnectionTypes, CoreError, CoreEvent, HostKeyPolicy, Next,
    OpenRequest, SecretSlot, SecretSource, SshAuth, SshHop, SshTunnel,
};
use ninesql_secrets::{
    SecretError, SecretId, SecretMeta, SecretRef, SecretStore, SecretStores, SecretString,
    StoreKind,
};
use support::*;

/// How a [`SlowStore`] answers `get`.
#[derive(Clone, Copy)]
enum Mode {
    /// Waits until [`SlowStore::release`] (an unlock prompt nobody answers).
    Hang,
    /// Panics inside the store (a client bug).
    Panic,
    /// Answers after this long.
    Delay(Duration),
}

struct SlowStore {
    mode: Mode,
    released: Mutex<bool>,
    wake: Condvar,
    calls: Mutex<u32>,
    finished: Mutex<u32>,
}

impl SlowStore {
    fn new(mode: Mode) -> Arc<Self> {
        Arc::new(Self {
            mode,
            released: Mutex::new(false),
            wake: Condvar::new(),
            calls: Mutex::new(0),
            finished: Mutex::new(0),
        })
    }

    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.wake.notify_all();
    }

    fn calls(&self) -> u32 {
        *self.calls.lock().unwrap()
    }

    fn finished(&self) -> u32 {
        *self.finished.lock().unwrap()
    }
}

impl SecretStore for SlowStore {
    fn kind(&self) -> StoreKind {
        StoreKind::Keyring
    }

    fn get(&self, id: &SecretId) -> Result<SecretString, SecretError> {
        *self.calls.lock().unwrap() += 1;
        if id.as_str().ends_with(":fast") {
            return Ok(SecretString::from("F4st-Answer".to_owned()));
        }
        match self.mode {
            Mode::Panic => panic!("simulated failure inside the secret store client"),
            Mode::Delay(d) => std::thread::sleep(d),
            Mode::Hang => {
                let mut released = self.released.lock().unwrap();
                while !*released {
                    released = self.wake.wait(released).unwrap();
                }
            }
        }
        *self.finished.lock().unwrap() += 1;
        Ok(SecretString::from("Unl0cked-Late".to_owned()))
    }

    fn set(&self, _: &SecretId, _: &SecretMeta, _: &SecretString) -> Result<(), SecretError> {
        Ok(())
    }

    fn delete(&self, _: &SecretId) -> Result<bool, SecretError> {
        Ok(false)
    }
}

fn saved_password_connection() -> ninesql_core::Connection {
    saved_password_connection_with("mock:auth")
}

fn saved_password_connection_with(id: &str) -> ninesql_core::Connection {
    let mut c = mock_connection(ConnectionTypeId::dev());
    c.auth = Auth::Password {
        username: "app".into(),
        secret: stored(id),
    };
    c
}

fn stored(id: &str) -> SecretSource {
    SecretSource::Stored(SecretRef::new(
        StoreKind::Keyring,
        SecretId::new(id).unwrap(),
    ))
}

async fn wait_until(what: &str, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ok() {
        assert!(Instant::now() < deadline, "timed out waiting: {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn stores_with(store: Arc<SlowStore>) -> SecretStores {
    let mut stores = SecretStores::new();
    stores.register(store);
    stores
}

#[test]
fn an_unanswered_store_call_times_out_as_unavailable() {
    let tmp = tempfile::tempdir().unwrap();
    let mock = mock_driver();
    let stats = mock.stats();
    let store = SlowStore::new(Mode::Hang);
    let manager = manager_with_stores(
        Arc::new(mock),
        tmp.path(),
        stores_with(store.clone()),
        |c| c.secret_timeout = Duration::from_millis(300),
    );
    let types = ConnectionTypes::default();
    ui(async {
        let started = Instant::now();
        let err = open(
            &manager,
            OpenRequest::new(saved_password_connection(), &types),
        )
        .await
        .unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(10));
        match &err {
            CoreError::Secret {
                slot: SecretSlot::Auth,
                source: SecretError::Unavailable { store, detail },
            } => {
                assert_eq!(*store, StoreKind::Keyring);
                assert!(detail.contains("no answer within"), "{detail}");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(err.kind(), "secret");
    });
    assert_eq!(store.calls(), 1);
    assert_eq!(stats.connects(), 0, "nothing connects without the secret");
    // The blocking thread is released by the store's own end (here: the prompt answered late);
    // its value is dropped unused.
    store.release();
    manager.shutdown(Duration::from_secs(5));
    assert_eq!(stats.connects(), 0);
}

#[test]
fn a_panicking_store_is_unavailable_not_a_runtime_error() {
    let tmp = tempfile::tempdir().unwrap();
    let mock = mock_driver();
    let stats = mock.stats();
    let store = SlowStore::new(Mode::Panic);
    let manager = manager_with_stores(
        Arc::new(mock),
        tmp.path(),
        stores_with(store.clone()),
        |_| {},
    );
    let types = ConnectionTypes::default();
    ui(async {
        let err = open(
            &manager,
            OpenRequest::new(saved_password_connection(), &types),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(
                err,
                CoreError::Secret {
                    slot: SecretSlot::Auth,
                    source: SecretError::Unavailable {
                        store: StoreKind::Keyring,
                        ..
                    },
                }
            ),
            "{err:?}"
        );
    });
    assert_eq!(stats.connects(), 0);
    manager.shutdown(Duration::from_secs(5));
}

#[test]
fn a_dropped_reply_abandons_the_open_while_the_store_waits() {
    let tmp = tempfile::tempdir().unwrap();
    let mock = mock_driver();
    let stats = mock.stats();
    let store = SlowStore::new(Mode::Hang);
    let manager = manager_with_stores(
        Arc::new(mock),
        tmp.path(),
        stores_with(store.clone()),
        // Far longer than the test: only the dropped reply can end the open.
        |c| c.secret_timeout = Duration::from_secs(600),
    );
    let mut events = manager.take_events().unwrap();
    let types = ConnectionTypes::default();
    ui(async {
        let reply = manager.open(OpenRequest::new(saved_password_connection(), &types));
        let deadline = Instant::now() + Duration::from_secs(10);
        while store.calls() == 0 {
            assert!(Instant::now() < deadline, "the store was never called");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        drop(reply);
        // Give the open task time to notice, then let the "prompt" answer.
        tokio::time::sleep(Duration::from_millis(200)).await;
        store.release();
        let deadline = Instant::now() + Duration::from_secs(10);
        while store.finished() == 0 {
            assert!(Instant::now() < deadline, "the store never finished");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // Had the open gone on, it would connect right after the store answered.
        tokio::time::sleep(Duration::from_millis(300)).await;
    });
    assert_eq!(
        stats.connects(),
        0,
        "the abandoned open never reached the driver"
    );
    match events.try_recv() {
        Next::Pending => {}
        Next::Ready(e @ CoreEvent::SessionOpened { .. }) => panic!("a session was opened: {e:?}"),
        other => panic!("unexpected event state: {other:?}"),
    }
    manager.shutdown(Duration::from_secs(5));
}

#[test]
fn the_default_secret_timeout_is_sane() {
    let tmp = tempfile::tempdir().unwrap();
    let config = ninesql_core::ManagerConfig::with_known_hosts(ninesql_core::KnownHostsFiles {
        own: tmp.path().join("known_hosts"),
        user: None,
    });
    assert_eq!(
        config.secret_timeout,
        ninesql_core::connect::DEFAULT_SECRET_TIMEOUT
    );
    assert!(config.secret_timeout >= Duration::from_secs(30));
    assert!(config.secret_timeout <= Duration::from_secs(300));
}

#[test]
fn opens_of_the_same_secret_join_the_read_in_flight() {
    let tmp = tempfile::tempdir().unwrap();
    let mock = mock_driver();
    let stats = mock.stats();
    let store = SlowStore::new(Mode::Hang);
    let manager = manager_with_stores(
        Arc::new(mock),
        tmp.path(),
        stores_with(store.clone()),
        |c| c.secret_timeout = Duration::from_millis(300),
    );
    let types = ConnectionTypes::default();
    ui(async {
        // Three opens (retries after a timeout) while the first store call still hangs.
        for _ in 0..3 {
            let err = open(
                &manager,
                OpenRequest::new(saved_password_connection(), &types),
            )
            .await
            .unwrap_err();
            assert!(matches!(
                err,
                CoreError::Secret {
                    source: SecretError::Unavailable { .. },
                    ..
                }
            ));
        }
        assert_eq!(store.calls(), 1, "one store call for the three opens");

        // Joined by opens that wait long enough: one answer serves them all.
        let replies: Vec<_> = (0..3)
            .map(|_| {
                let mut c = saved_password_connection();
                c.id = ninesql_core::ConnectionId::new_random();
                manager.open(OpenRequest::new(c, &types))
            })
            .collect();
        tokio::time::sleep(Duration::from_millis(100)).await;
        store.release();
        for r in replies {
            let _ = tokio::time::timeout(Duration::from_secs(10), r)
                .await
                .unwrap();
        }
    });
    // The joined opens used the 300 ms deadline too, so some may have timed out; never a second
    // store call while the first was in flight.
    assert_eq!(store.calls(), 1);
    assert!(stats.connects() <= 3);
    manager.shutdown(Duration::from_secs(5));
}

#[test]
fn joined_opens_get_the_value_once_the_store_answers() {
    let tmp = tempfile::tempdir().unwrap();
    let mock = mock_driver();
    let stats = mock.stats();
    let store = SlowStore::new(Mode::Hang);
    let manager = manager_with_stores(
        Arc::new(mock),
        tmp.path(),
        stores_with(store.clone()),
        |c| c.secret_timeout = Duration::from_secs(30),
    );
    let types = ConnectionTypes::default();
    ui(async {
        let replies: Vec<_> = (0..3)
            .map(|_| manager.open(OpenRequest::new(saved_password_connection(), &types)))
            .collect();
        wait_until("first store call", || store.calls() == 1).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        store.release();
        for r in replies {
            let outcome = tokio::time::timeout(Duration::from_secs(10), r)
                .await
                .unwrap()
                .unwrap();
            assert!(
                matches!(outcome, Ok(ninesql_core::OpenOutcome::Opened(_))),
                "{outcome:?}"
            );
        }
    });
    assert_eq!(store.calls(), 1);
    assert_eq!(stats.connects(), 3);
    manager.shutdown(Duration::from_secs(5));
}

#[test]
fn pending_store_reads_are_capped() {
    let tmp = tempfile::tempdir().unwrap();
    let mock = mock_driver();
    let store = SlowStore::new(Mode::Hang);
    let manager = manager_with_stores(
        Arc::new(mock),
        tmp.path(),
        stores_with(store.clone()),
        |c| c.secret_timeout = Duration::from_secs(30),
    );
    let types = ConnectionTypes::default();
    let max = u32::try_from(ninesql_core::connect::MAX_PENDING_SECRET_READS).unwrap();
    ui(async {
        let replies: Vec<_> = (0..max)
            .map(|i| {
                manager.open(OpenRequest::new(
                    saved_password_connection_with(&format!("mock:auth-{i}")),
                    &types,
                ))
            })
            .collect();
        wait_until("every read started", || store.calls() == max).await;
        let started = Instant::now();
        let err = open(
            &manager,
            OpenRequest::new(saved_password_connection_with("mock:one-more"), &types),
        )
        .await
        .unwrap_err();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "refused at once"
        );
        match err {
            CoreError::Secret {
                source: SecretError::Unavailable { detail, .. },
                ..
            } => assert!(detail.contains("too many"), "{detail}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(store.calls(), max, "no store call for the refused read");
        store.release();
        for r in replies {
            let _ = tokio::time::timeout(Duration::from_secs(10), r)
                .await
                .unwrap();
        }
    });
    manager.shutdown(Duration::from_secs(5));
}

#[test]
fn several_fields_share_one_deadline() {
    let tmp = tempfile::tempdir().unwrap();
    let mock = mock_driver();
    let stats = mock.stats();
    // Each read takes 250 ms: the first fits in the 400 ms deadline, the second does not.
    let store = SlowStore::new(Mode::Delay(Duration::from_millis(250)));
    let manager = manager_with_stores(
        Arc::new(mock),
        tmp.path(),
        stores_with(store.clone()),
        |c| c.secret_timeout = Duration::from_millis(400),
    );
    let types = ConnectionTypes::default();
    let mut c = saved_password_connection();
    let mut server = SshHop::new(
        "127.0.0.1",
        22,
        "deploy",
        SshAuth::Password {
            secret: stored("mock:ssh"),
        },
    );
    server.host_key_policy = HostKeyPolicy::Strict;
    c.ssh = Some(SshTunnel {
        server,
        jump_hosts: vec![],
    });
    ui(async {
        let err = open(&manager, OpenRequest::new(c, &types))
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                CoreError::Secret {
                    slot: SecretSlot::SshServer,
                    source: SecretError::Unavailable { .. },
                }
            ),
            "{err:?}"
        );
    });
    assert_eq!(store.calls(), 2);
    assert_eq!(stats.connects(), 0);
    manager.shutdown(Duration::from_secs(5));
}

#[test]
fn another_secret_resolves_while_one_hangs() {
    let tmp = tempfile::tempdir().unwrap();
    let mock = mock_driver();
    let stats = mock.stats();
    let store = SlowStore::new(Mode::Hang);
    let manager = manager_with_stores(
        Arc::new(mock),
        tmp.path(),
        stores_with(store.clone()),
        |c| c.secret_timeout = Duration::from_secs(30),
    );
    let types = ConnectionTypes::default();
    ui(async {
        let hanging = manager.open(OpenRequest::new(saved_password_connection(), &types));
        wait_until("hanging read started", || store.calls() == 1).await;
        // Below the cap, a different secret is read and the open completes meanwhile.
        let outcome = open(
            &manager,
            OpenRequest::new(saved_password_connection_with("mock:fast"), &types),
        )
        .await
        .unwrap();
        assert!(
            matches!(outcome, ninesql_core::OpenOutcome::Opened(_)),
            "{outcome:?}"
        );
        assert_eq!(store.calls(), 2);
        assert_eq!(stats.connects(), 1);
        store.release();
        let _ = tokio::time::timeout(Duration::from_secs(10), hanging)
            .await
            .unwrap();
    });
    manager.shutdown(Duration::from_secs(5));
}
