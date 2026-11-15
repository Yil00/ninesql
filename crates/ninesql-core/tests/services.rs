//! Startup assembly (`CoreServices`, wave 2 integration review L5): secret backend selection,
//! the vault unlock flow of `open` (`OpenOutcome::NeedsVaultUnlock`), lock hooks and the idle
//! timer, validation at save, the password-exchange field and the DBeaver file flag (I4), and
//! the I1 checks on the master password.
#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).

mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use ninesql_core::driver_api::{self as api, Secret};
use ninesql_core::{
    Auth, ConnectionError, ConnectionTypeId, ConnectionTypes, Connections, CoreConfig, CoreError,
    CoreEvent, CoreServices, DriverRegistry, Endpoint, KnownHostsFiles, OpenOutcome, OpenRequest,
    PasswordExchange, SecretBackend, SecretSlot, SecretSource, SecretsLockReason, TlsMode,
    WorkspaceError,
};
use ninesql_secrets::{
    CacheLifetime, KdfParams, SecretId, SecretString, StoreKind, VaultError, VaultStatus,
};
use ninesql_storage::{Paths, ProjectName, XdgDirs};
use support::*;

const MASTER: &str = "Ma5ter-Pa55-6c0d2e";
const DB_PASSWORD: &str = "Db-Pa55-a81f";

fn paths(root: &Path) -> Paths {
    Paths::new(&XdgDirs {
        config_home: root.join("config"),
        data_home: root.join("data"),
        state_home: root.join("state"),
    })
}

fn config(root: &Path, driver: Arc<dyn api::Driver>) -> CoreConfig {
    let paths = paths(root);
    let mut registry = DriverRegistry::new();
    registry.register(driver);
    let mut c = CoreConfig::new(paths.clone(), ProjectName::default_project(), registry);
    c.manager.known_hosts = KnownHostsFiles {
        own: paths.data_dir().join("known_hosts"),
        user: None,
    };
    c.manager.workers = 2;
    c.manager.dbeaver_roots = ninesql_core::DbeaverRoots::none();
    // No real Secret Service or logind in tests.
    c.use_secret_service = false;
    c.watch_desktop_lock = false;
    c
}

fn start(c: CoreConfig) -> CoreServices {
    ui(async {
        tokio::time::timeout(Duration::from_secs(20), CoreServices::start(c))
            .await
            .unwrap()
            .unwrap()
            .unwrap()
    })
}

fn s(v: &str) -> SecretString {
    SecretString::from(v.to_owned())
}

fn all_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out
}

fn assert_not_on_disk(root: &Path, needles: &[&str]) {
    for file in all_files(root) {
        let bytes = std::fs::read(&file).unwrap();
        for needle in needles {
            assert!(
                !bytes.windows(needle.len()).any(|w| w == needle.as_bytes()),
                "{needle} found in {}",
                file.display()
            );
        }
    }
}

async fn next_lock_event(events: &mut ninesql_core::CoreEvents) -> SecretsLockReason {
    loop {
        if let CoreEvent::SecretsLocked { reason } =
            tokio::time::timeout(Duration::from_secs(10), events.recv())
                .await
                .expect("no event")
                .expect("channel closed")
        {
            return reason;
        }
    }
}

#[test]
fn startup_selects_the_vault_without_a_keyring_and_reports_the_watcher() {
    let tmp = tempfile::tempdir().unwrap();
    let services = start(config(tmp.path(), Arc::new(mock_driver())));
    assert_eq!(services.secret_backend(), SecretBackend::Vault);
    assert_eq!(services.secret_backend().store(), Some(StoreKind::Vault));
    assert!(services.secret_stores().has(StoreKind::Vault));
    assert!(!services.secret_stores().has(StoreKind::Keyring));
    assert!(services.vault_error().is_none());
    // The vault and the cache are registered, plus the event hook.
    assert_eq!(services.lock_hooks().len(), 3);
    let status = services.lock_watcher_status();
    assert!(!status.is_running);
    assert!(status.error.is_some());
    assert_eq!(status.built, cfg!(feature = "desktop-lock"));
    ui(async {
        let st = services.vault_status().await.unwrap().unwrap();
        assert_eq!(st, VaultStatus::NotCreated);
    });
    assert!(services.take_events().is_some());
    assert!(services.take_events().is_none(), "one receiver");
    services.shutdown(Duration::from_secs(2));
}

/// Without a system bus (the sandbox), starting the watcher fails fast and is not an error of
/// the startup; with a bus it runs. Either way the status says which.
#[cfg(feature = "desktop-lock")]
#[test]
fn the_watcher_is_started_when_asked_and_its_failure_is_reported() {
    let tmp = tempfile::tempdir().unwrap();
    let mut c = config(tmp.path(), Arc::new(mock_driver()));
    c.watch_desktop_lock = true;
    let services = start(c);
    let status = services.lock_watcher_status();
    assert!(status.built);
    assert_eq!(status.is_running, status.error.is_none(), "{status:?}");
}

#[test]
fn startup_fails_cleanly_when_the_workspace_cannot_be_locked() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = paths(tmp.path());
    // A directory in place of the workspace lock file.
    std::fs::create_dir_all(paths.lock_file()).unwrap();
    let err = CoreServices::start_blocking(config(tmp.path(), Arc::new(mock_driver())));
    assert!(
        matches!(err, Err(ninesql_core::StartupError::Workspace(_))),
        "{err:?}"
    );
}

#[test]
fn a_locked_vault_asks_for_unlock_then_opens_and_the_master_password_never_reaches_disk() {
    let tmp = tempfile::tempdir().unwrap();
    let wrap = Wrap::new(mock_driver().with_password(Secret::new(DB_PASSWORD.into())));
    let services = start(config(tmp.path(), Arc::new(wrap.clone())));
    let mut events = services.take_events().unwrap();
    let types = ConnectionTypes::default();

    ui(async {
        // First use: create the vault, then save the password into it.
        services
            .create_vault_with(s(MASTER), KdfParams::insecure_for_tests())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            services.create_vault(s(MASTER)).await.unwrap(),
            Err(CoreError::Vault(VaultError::AlreadyExists(_)))
        ));
    });
    let mut connections = Connections::default();
    let mut c = mock_connection(ConnectionTypeId::dev());
    c.auth = Auth::Password {
        username: "app".into(),
        secret: SecretSource::Prompt,
    };
    let id = services
        .workspace()
        .create_connection_checked(
            &mut connections,
            c,
            vec![(SecretSlot::Auth, s(DB_PASSWORD))],
            services.secret_backend().store().unwrap(),
            services.secret_stores(),
            services.registry(),
        )
        .unwrap();
    let c = connections.get(id).unwrap().clone();

    ui(async {
        // "Lock secrets": vault locked, cache cleared, event reported.
        services.secret_cache().insert(
            SecretId::new(format!("{id}:auth")).unwrap(),
            s("typed"),
            CacheLifetime::Session,
        );
        services.lock_now().await.unwrap();
        assert_eq!(
            next_lock_event(&mut events).await,
            SecretsLockReason::Requested
        );
        assert!(services.secret_cache().is_empty());
        assert_eq!(
            services.vault_status().await.unwrap().unwrap(),
            VaultStatus::Locked
        );

        // The open asks for the vault, without touching the driver.
        let out = open(services.sessions(), OpenRequest::new(c.clone(), &types))
            .await
            .unwrap();
        assert!(
            matches!(out, OpenOutcome::NeedsVaultUnlock { connection, slot: SecretSlot::Auth } if connection == id),
            "{out:?}"
        );
        assert!(wrap.seen.lock().unwrap().is_empty());

        // A wrong master password: typed error, never echoed.
        let err = services
            .unlock_vault(s("not-the-master"))
            .await
            .unwrap()
            .unwrap_err();
        assert!(
            matches!(err, CoreError::Vault(VaultError::WrongPassword)),
            "{err:?}"
        );
        assert_eq!(err.kind(), "vault-wrong-password");
        assert!(!format!("{err} {err:?}").contains("not-the-master"));

        // Unlock, open again: connected with the vault password.
        services.unlock_vault(s(MASTER)).await.unwrap().unwrap();
        let out = open(services.sessions(), OpenRequest::new(c.clone(), &types))
            .await
            .unwrap();
        let OpenOutcome::Opened(h) = out else {
            panic!("{out:?}");
        };
        assert!(wrap.last_seen().has_password);
        h.close().await.unwrap().unwrap();
    });

    let debug = format!("{services:?}");
    assert!(
        !debug.contains(MASTER) && !debug.contains(DB_PASSWORD),
        "{debug}"
    );
    services.shutdown(Duration::from_secs(2));
    // I1: neither the master password nor the database password is on disk in clear.
    assert_not_on_disk(tmp.path(), &[MASTER, DB_PASSWORD]);
    assert!(paths(tmp.path()).vault_file().exists());
}

#[test]
fn the_idle_timer_locks_the_vault_and_reports_it() {
    let tmp = tempfile::tempdir().unwrap();
    let mut c = config(tmp.path(), Arc::new(mock_driver()));
    c.vault_idle_timeout = Some(Duration::from_millis(200));
    c.lock_check_interval = Duration::from_millis(100);
    let services = start(c);
    let mut events = services.take_events().unwrap();
    ui(async {
        services
            .create_vault_with(s(MASTER), KdfParams::insecure_for_tests())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(next_lock_event(&mut events).await, SecretsLockReason::Idle);
        assert_eq!(
            services.vault_status().await.unwrap().unwrap(),
            VaultStatus::Locked
        );
    });
}

#[test]
fn hooks_triggered_like_the_watcher_lock_everything_and_report_the_reason() {
    let tmp = tempfile::tempdir().unwrap();
    let services = start(config(tmp.path(), Arc::new(mock_driver())));
    let mut events = services.take_events().unwrap();
    ui(async {
        services
            .create_vault_with(s(MASTER), KdfParams::insecure_for_tests())
            .await
            .unwrap()
            .unwrap();
        // What `DesktopLockWatcher` calls on `PrepareForSleep(true)`.
        services
            .lock_hooks()
            .lock_all(ninesql_secrets::LockTrigger::Sleep);
        assert_eq!(next_lock_event(&mut events).await, SecretsLockReason::Sleep);
        assert_eq!(
            services.vault_status().await.unwrap().unwrap(),
            VaultStatus::Locked
        );
    });
}

#[test]
fn save_refuses_what_the_driver_cannot_do() {
    let tmp = tempfile::tempdir().unwrap();
    // A driver without SCRAM-only and with only verify-full TLS.
    let mut caps = mock_capabilities();
    caps.connect.password_exchanges = [api::PasswordExchange::Any].into_iter().collect();
    caps.tls = [api::TlsMode::Disable, api::TlsMode::VerifyFull]
        .into_iter()
        .collect();
    let services = start(config(
        tmp.path(),
        Arc::new(mock_driver().with_capabilities(caps)),
    ));
    let ok = mock_connection(ConnectionTypeId::dev());
    services.validate_connection(&ok).unwrap();

    let field_of = |c: &ninesql_core::Connection| match services.validate_connection(c) {
        Err(CoreError::InvalidConnection(ConnectionError::InvalidField { field, .. })) => field,
        other => panic!("{other:?}"),
    };
    let mut scram = ok.clone();
    scram.id = ninesql_core::ConnectionId::new_random();
    scram.password_exchange = PasswordExchange::ScramOnly;
    assert_eq!(field_of(&scram), "password_exchange");
    let mut require = ok.clone();
    require.tls.mode = TlsMode::Require;
    assert_eq!(field_of(&require), "tls.mode");
    // A file endpoint ignores the TLS mode.
    let mut file = ok.clone();
    file.endpoint = Endpoint::File {
        path: "/tmp/x.db".into(),
    };
    file.tls.mode = TlsMode::Require;
    services.validate_connection(&file).unwrap();

    // The checked save writes nothing when one connection is refused.
    let connections = Connections::from_vec(vec![ok.clone(), scram]).unwrap();
    let err = services
        .workspace()
        .save_connections_checked(&connections, services.registry())
        .unwrap_err();
    assert!(matches!(err, WorkspaceError::Core { .. }), "{err:?}");
    assert!(
        !services
            .workspace()
            .project_paths()
            .connections_file()
            .exists()
    );
    // A disabled connection of an engine without a driver is saved (kept for later).
    let mut later = ok.clone();
    later.id = ninesql_core::ConnectionId::new_random();
    later.engine = ninesql_core::EngineId::new("cassandra").unwrap();
    later.enabled = false;
    let connections = Connections::from_vec(vec![ok, later]).unwrap();
    services
        .workspace()
        .save_connections_checked(&connections, services.registry())
        .unwrap();
}

#[test]
fn a_driver_without_tls_accepts_only_disable_on_network_endpoints() {
    let mut caps = mock_capabilities();
    caps.tls.clear();
    let mut c = mock_connection(ConnectionTypeId::dev());
    assert!(matches!(
        c.validate_for_driver(&caps),
        Err(ConnectionError::InvalidField { field, .. }) if field == "tls.mode"
    ));
    c.tls.mode = TlsMode::Disable;
    c.validate_for_driver(&caps).unwrap();

    // A driver that declares TLS modes but not `disable` refuses it.
    let mut tls_only = mock_capabilities();
    tls_only.tls = [api::TlsMode::VerifyFull].into_iter().collect();
    assert!(matches!(
        c.validate_for_driver(&tls_only),
        Err(ConnectionError::InvalidField { field, .. }) if field == "tls.mode"
    ));
}

#[test]
fn the_password_exchange_and_the_dbeaver_flag_reach_the_driver() {
    let tmp = tempfile::tempdir().unwrap();
    let wrap = Wrap::new(mock_driver());
    let mut c = config(tmp.path(), Arc::new(wrap.clone()));
    let dbeaver_ws = tmp
        .path()
        .join("home/.local/share/DBeaverData/workspace6/General");
    std::fs::create_dir_all(&dbeaver_ws).unwrap();
    c.manager.dbeaver_roots =
        ninesql_core::DbeaverRoots::from_search_roots(&ninesql_core::dbeaver::SearchRoots {
            home: tmp.path().join("home"),
        });
    let services = start(c);
    let types = ConnectionTypes::default();
    ui(async {
        let mut scram = mock_connection(ConnectionTypeId::dev());
        scram.password_exchange = PasswordExchange::ScramOnly;
        let h = open_ok(services.sessions(), scram, &types).await;
        let seen = wrap.last_seen();
        assert_eq!(seen.password_exchange, api::PasswordExchange::ScramOnly);
        assert!(!seen.inside_dbeaver_workspace, "network endpoint");
        h.close().await.unwrap().unwrap();

        let mut inside = mock_connection(ConnectionTypeId::dev());
        inside.endpoint = Endpoint::File {
            path: dbeaver_ws.join("sample.db"),
        };
        let h = open_ok(services.sessions(), inside, &types).await;
        assert!(wrap.last_seen().inside_dbeaver_workspace);
        // The UI can say why the file was opened read-only (review P1-D wiring, L3).
        assert!(h.info().inside_dbeaver_workspace);
        assert_eq!(
            wrap.last_seen().password_exchange,
            api::PasswordExchange::Any
        );
        h.close().await.unwrap().unwrap();

        let mut outside = mock_connection(ConnectionTypeId::dev());
        outside.endpoint = Endpoint::File {
            path: tmp.path().join("data.db"),
        };
        let h = open_ok(services.sessions(), outside, &types).await;
        assert!(!wrap.last_seen().inside_dbeaver_workspace);
        assert!(!h.info().inside_dbeaver_workspace);
        h.close().await.unwrap().unwrap();

        // The driver gets the canonical path that was checked (review P1-D wiring, L5).
        let real = tmp.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(&real, tmp.path().join("via-link")).unwrap();
        let mut linked = mock_connection(ConnectionTypeId::dev());
        linked.endpoint = Endpoint::File {
            path: tmp.path().join("via-link/../via-link/x.db"),
        };
        let h = open_ok(services.sessions(), linked, &types).await;
        let api::Endpoint::File { path, .. } = wrap.last_seen().endpoint else {
            panic!("not a file endpoint");
        };
        assert_eq!(path, std::fs::canonicalize(&real).unwrap().join("x.db"));
        h.close().await.unwrap().unwrap();

        // A folder picked for an import, added after startup.
        let picked = tmp.path().join("elsewhere/ws");
        services.sessions().dbeaver_roots().add(&picked);
        let mut later = mock_connection(ConnectionTypeId::dev());
        later.endpoint = Endpoint::File {
            path: picked.join("p/db.sqlite"),
        };
        let h = open_ok(services.sessions(), later, &types).await;
        assert!(wrap.last_seen().inside_dbeaver_workspace);
        h.close().await.unwrap().unwrap();
    });
}

/// P1-E review (for P2-C): once the user confirms imported init SQL, the core clears the
/// marker in `connections.json`, only when the init SQL on disk is still what was confirmed.
#[test]
fn confirming_imported_init_sql_clears_the_marker_on_disk() {
    let tmp = tempfile::tempdir().unwrap();
    let wrap = Wrap::new(mock_driver());
    let services = start(config(tmp.path(), Arc::new(wrap.clone())));
    let types = ConnectionTypes::default();
    let mut c = mock_connection(ConnectionTypeId::dev());
    c.init_sql = vec!["SET search_path TO app".into()];
    c.init_sql_needs_confirmation = true;
    let mut other = mock_connection(ConnectionTypeId::dev());
    other.name = "Other".into();
    other.init_sql = vec!["SELECT * FROM numbers(1)".into()];
    other.init_sql_needs_confirmation = true;
    let ws = services.workspace();
    ws.save_connections_checked(
        &Connections::from_vec(vec![c.clone(), other.clone()]).unwrap(),
        services.registry(),
    )
    .unwrap();
    let raw = std::fs::read_to_string(ws.project_paths().connections_file()).unwrap();
    assert!(
        raw.contains("\"init_sql_needs_confirmation\": true"),
        "{raw}"
    );
    let marker = |id| {
        ws.load_connections()
            .unwrap()
            .connections
            .get(id)
            .unwrap()
            .init_sql_needs_confirmation
    };

    ui(async {
        // A stale request (init SQL changed on disk since): confirmed, but the marker of the
        // statements on disk stays.
        let mut stale = other.clone();
        stale.init_sql = vec!["SELECT * FROM numbers(2)".into()];
        let OpenOutcome::InitSqlNeedsConfirmation(ic) =
            open(services.sessions(), OpenRequest::new(stale.clone(), &types))
                .await
                .unwrap()
        else {
            panic!("no confirmation");
        };
        let h = match open(
            services.sessions(),
            OpenRequest::new(stale, &types).with_init_sql_confirmation(ic.token),
        )
        .await
        .unwrap()
        {
            OpenOutcome::Opened(h) => h,
            out => panic!("{out:?}"),
        };
        h.close().await.unwrap().unwrap();
        assert!(marker(other.id), "other statements than the ones on disk");

        let OpenOutcome::InitSqlNeedsConfirmation(ic) =
            open(services.sessions(), OpenRequest::new(c.clone(), &types))
                .await
                .unwrap()
        else {
            panic!("no confirmation");
        };
        assert!(ic.imported);
        let h = match open(
            services.sessions(),
            OpenRequest::new(c.clone(), &types).with_init_sql_confirmation(ic.token),
        )
        .await
        .unwrap()
        {
            OpenOutcome::Opened(h) => h,
            out => panic!("{out:?}"),
        };
        h.close().await.unwrap().unwrap();
    });
    assert!(!marker(c.id), "cleared once confirmed");
    assert!(marker(other.id), "only the confirmed connection");
    let raw = std::fs::read_to_string(ws.project_paths().connections_file()).unwrap();
    assert_eq!(raw.matches("init_sql_needs_confirmation").count(), 1);

    // The connection as saved now opens without confirmation (dev type, harmless statement).
    let reloaded = ws
        .load_connections()
        .unwrap()
        .connections
        .get(c.id)
        .cloned();
    ui(async {
        let h = open_ok(services.sessions(), reloaded.unwrap(), &types).await;
        h.close().await.unwrap().unwrap();
    });
}

/// P1-E review, M2: the core reads the marker from `connections.json`, not only from the
/// request; init SQL that differs from the saved one counts as not confirmed. L1 / M1: the event
/// says what was confirmed and whether the file holds it cleared.
#[test]
fn the_marker_on_disk_wins_over_the_request() {
    let tmp = tempfile::tempdir().unwrap();
    let wrap = Wrap::new(mock_driver());
    let services = start(config(tmp.path(), Arc::new(wrap.clone())));
    let mut events = services.take_events().unwrap();
    let types = ConnectionTypes::default();
    let mut c = mock_connection(ConnectionTypeId::dev());
    c.init_sql = vec!["SET search_path TO app".into()];
    c.init_sql_needs_confirmation = true;
    let ws = services.workspace();
    ws.save_connections_checked(
        &Connections::from_vec(vec![c.clone()]).unwrap(),
        services.registry(),
    )
    .unwrap();

    ui(async {
        // A request that lost the marker still asks, as imported.
        let mut stale = c.clone();
        stale.init_sql_needs_confirmation = false;
        let OpenOutcome::InitSqlNeedsConfirmation(ic) =
            open(services.sessions(), OpenRequest::new(stale.clone(), &types))
                .await
                .unwrap()
        else {
            panic!("the marker on disk was not read");
        };
        assert!(ic.imported);
        // Init SQL changed since it was saved, on an unmarked request: asks too, and confirming
        // it leaves the marker of the saved statements.
        let mut changed = stale.clone();
        changed.init_sql = vec!["SET search_path TO other".into()];
        let OpenOutcome::InitSqlNeedsConfirmation(ic2) = open(
            services.sessions(),
            OpenRequest::new(changed.clone(), &types),
        )
        .await
        .unwrap() else {
            panic!("changed init SQL was not asked");
        };
        assert!(ic2.imported);
        let h = match open(
            services.sessions(),
            OpenRequest::new(changed.clone(), &types).with_init_sql_confirmation(ic2.token),
        )
        .await
        .unwrap()
        {
            OpenOutcome::Opened(h) => h,
            out => panic!("{out:?}"),
        };
        h.close().await.unwrap().unwrap();
        let ev = next_confirmed(&mut events).await;
        assert_eq!(
            ev,
            CoreEvent::ImportedInitSqlConfirmed {
                connection: c.id,
                statements: changed.init_sql.clone(),
                cleared: Some(false),
            }
        );

        // Confirming the saved statements (from the stale request) clears the file.
        let h = match open(
            services.sessions(),
            OpenRequest::new(stale.clone(), &types).with_init_sql_confirmation(ic.token),
        )
        .await
        .unwrap()
        {
            OpenOutcome::Opened(h) => h,
            out => panic!("{out:?}"),
        };
        h.close().await.unwrap().unwrap();
        let ev = next_confirmed(&mut events).await;
        assert_eq!(
            ev,
            CoreEvent::ImportedInitSqlConfirmed {
                connection: c.id,
                statements: c.init_sql.clone(),
                cleared: Some(true),
            }
        );
        // Now the unmarked request opens without confirmation.
        let h = open_ok(services.sessions(), stale, &types).await;
        h.close().await.unwrap().unwrap();
    });
    let saved = ws
        .load_connections()
        .unwrap()
        .connections
        .get(c.id)
        .cloned();
    assert!(!saved.unwrap().init_sql_needs_confirmation);
}

async fn next_confirmed(events: &mut ninesql_core::CoreEvents) -> CoreEvent {
    loop {
        match tokio::time::timeout(Duration::from_secs(5), events.recv()).await {
            Ok(Some(ev @ CoreEvent::ImportedInitSqlConfirmed { .. })) => return ev,
            Ok(Some(_)) => {}
            other => panic!("no ImportedInitSqlConfirmed: {other:?}"),
        }
    }
}

/// P1-E review, L2 (I4, defence in depth): a file inside a DBeaver workspace is also opened as
/// a read-only session, and a driver that ignores both flags (a future file driver) is refused.
#[test]
fn a_dbeaver_file_on_a_driver_that_ignores_read_only_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let mut wrap = Wrap::new(mock_driver());
    wrap.ignore_read_only = true;
    wrap.ignore_dbeaver_flag = true;
    let mut c = config(tmp.path(), Arc::new(wrap.clone()));
    let dbeaver_ws = tmp
        .path()
        .join("home/.local/share/DBeaverData/workspace6/General");
    std::fs::create_dir_all(&dbeaver_ws).unwrap();
    c.manager.dbeaver_roots =
        ninesql_core::DbeaverRoots::from_search_roots(&ninesql_core::dbeaver::SearchRoots {
            home: tmp.path().join("home"),
        });
    let services = start(c);
    let types = ConnectionTypes::default();
    ui(async {
        // A writable type: only the DBeaver location asks for read-only.
        let mut inside = mock_connection(ConnectionTypeId::dev());
        inside.endpoint = Endpoint::File {
            path: dbeaver_ws.join("sample.db"),
        };
        let inside_id = inside.id;
        let err = open(services.sessions(), OpenRequest::new(inside, &types))
            .await
            .unwrap_err();
        assert!(matches!(err, CoreError::ReadOnlyNotEnforced), "{err:?}");
        let seen = wrap.last_seen();
        assert!(seen.read_only, "the core asked for a read-only session");
        assert!(seen.inside_dbeaver_workspace);
        assert!(services.sessions().sessions_of(inside_id).is_empty());

        // Outside a DBeaver workspace the same driver opens a writable dev session.
        let mut outside = mock_connection(ConnectionTypeId::dev());
        outside.endpoint = Endpoint::File {
            path: tmp.path().join("data.db"),
        };
        let h = open_ok(services.sessions(), outside, &types).await;
        assert!(!wrap.last_seen().read_only);
        assert!(!h.info().read_only);
        h.close().await.unwrap().unwrap();
    });
}

/// Review P1-D wiring, L8 / L9: with the vault locked, `open` asks for the unlock before it
/// asks for prompted secrets, and before anything dials the SSH hop.
#[test]
fn the_vault_unlock_comes_before_prompts_and_before_any_ssh_dial() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let tmp = tempfile::tempdir().unwrap();
    let wrap = Wrap::new(mock_driver().with_password(Secret::new(DB_PASSWORD.into())));
    let mut cfg = config(tmp.path(), Arc::new(wrap.clone()));
    cfg.manager.connect_timeout = Duration::from_secs(3);
    let services = start(cfg);
    let types = ConnectionTypes::default();

    // A stand-in SSH server that records every connection and closes it at once.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let dials = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&dials);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            counter.fetch_add(1, Ordering::SeqCst);
            drop(stream);
        }
    });

    ui(async {
        services
            .create_vault_with(s(MASTER), KdfParams::insecure_for_tests())
            .await
            .unwrap()
            .unwrap();
    });
    let mut c = mock_connection(ConnectionTypeId::dev());
    c.auth = Auth::Password {
        username: "app".into(),
        secret: SecretSource::Prompt,
    };
    c.ssh = Some(ninesql_core::SshTunnel {
        server: ninesql_core::SshHop::new(
            "127.0.0.1",
            port,
            "deploy",
            ninesql_core::SshAuth::Password {
                secret: SecretSource::Prompt,
            },
        ),
        jump_hosts: vec![],
    });
    let mut connections = Connections::default();
    // The database password goes to the vault; the SSH password stays "ask every time".
    let id = services
        .workspace()
        .create_connection_checked(
            &mut connections,
            c,
            vec![(SecretSlot::Auth, s(DB_PASSWORD))],
            StoreKind::Vault,
            services.secret_stores(),
            services.registry(),
        )
        .unwrap();
    let c = connections.get(id).unwrap().clone();

    ui(async {
        services.lock_now().await.unwrap();
        // L8: the unlock is asked for first, not the SSH password.
        let out = open(services.sessions(), OpenRequest::new(c.clone(), &types))
            .await
            .unwrap();
        assert!(
            matches!(
                out,
                OpenOutcome::NeedsVaultUnlock {
                    slot: SecretSlot::Auth,
                    ..
                }
            ),
            "{out:?}"
        );
        // L9: with every prompt answered, still the unlock first, and no dial.
        let request =
            OpenRequest::new(c.clone(), &types).with_secret(SecretSlot::SshServer, s("ssh-pw"));
        let out = open(services.sessions(), request).await.unwrap();
        assert!(
            matches!(
                out,
                OpenOutcome::NeedsVaultUnlock {
                    slot: SecretSlot::Auth,
                    ..
                }
            ),
            "{out:?}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(dials.load(Ordering::SeqCst), 0, "the SSH hop was dialled");
        assert!(wrap.seen.lock().unwrap().is_empty());

        // Unlocked: now the SSH password is asked for…
        services.unlock_vault(s(MASTER)).await.unwrap().unwrap();
        let out = open(services.sessions(), OpenRequest::new(c.clone(), &types))
            .await
            .unwrap();
        let OpenOutcome::NeedsSecrets(prompts) = out else {
            panic!("{out:?}");
        };
        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0].slot, SecretSlot::SshServer);
        // …and with it the hop is dialled (the stand-in is no SSH server: the open fails).
        let request =
            OpenRequest::new(c.clone(), &types).with_secret(SecretSlot::SshServer, s("ssh-pw"));
        assert!(open(services.sessions(), request).await.is_err());
        assert!(
            dials.load(Ordering::SeqCst) >= 1,
            "the listener records dials"
        );
    });
    services.shutdown(Duration::from_secs(2));
}
