//! `TestAppContext` tests of the connection UI: actions, the dialog, saving (I1), the
//! navigator's folders, duplicate / delete, and the connect flow against the core's mock driver
//! (`NeedsSecrets`, `InitSqlNeedsConfirmation`, `Opened`, `SessionClosed`, production frame).

use std::sync::Arc;
use std::time::Duration;

use gpui::{Entity, TestAppContext, VisualTestContext};
use gpui_kit::component::WindowExt as _;
use ninesql_core::driver_api::mock::MockDriver;
use ninesql_core::secrets::{MemoryStore, SecretStores};
use ninesql_core::storage::{Paths, ProjectName, XdgDirs};
use ninesql_core::{
    Auth, Connection, ConnectionTypeId, Endpoint, EngineId, KnownHostsFiles, ManagerConfig,
    SecretSource, SessionManager, TlsMode, Workspace,
};

use super::connect_flow::{FlowResult, PendingPrompt, Purpose};
use super::form::SecretMode;
use super::hub::{ConnectionHub, HubConfig, install, registry_with};
use crate::app;
use crate::settings::{self, SettingsState};
use crate::workspace::Workspace as Shell;

fn shipped() -> ninesql_core::DriverRegistry {
    registry_with(vec![
        Arc::new(ninesql_driver_pgwire::PgWireDriver::new()),
        Arc::new(ninesql_driver_mysqlwire::MySqlWireDriver::new()),
        Arc::new(ninesql_driver_sqlite::SqliteDriver::new()),
    ])
}

fn init(cx: &mut TestAppContext, config: HubConfig) -> Entity<ConnectionHub> {
    let (settings, diagnostics) = settings::parse("");
    cx.update(|cx| {
        app::init(
            SettingsState {
                path: None,
                settings,
                diagnostics,
                save_blocked: None,
            },
            cx,
        );
        install(config, cx)
    })
}

fn open(cx: &mut TestAppContext) -> (Entity<Shell>, VisualTestContext) {
    let Some((window, ws)) = cx.update(app::open_window) else {
        panic!("the test platform opens windows");
    };
    let vcx = VisualTestContext::from_window(window, cx);
    vcx.run_until_parked();
    (ws, vcx)
}

/// Runs the foreground until `done`, letting the driver runtime's threads work in between.
fn wait_until(vcx: &mut VisualTestContext, mut done: impl FnMut(&mut VisualTestContext) -> bool) {
    vcx.executor().allow_parking();
    for _ in 0..500 {
        vcx.run_until_parked();
        if done(vcx) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("condition not reached");
}

fn set(
    input: Option<&Entity<gpui_kit::component::input::InputState>>,
    value: &str,
    vcx: &mut VisualTestContext,
) {
    let Some(input) = input.cloned() else {
        panic!("no such input");
    };
    let value = value.to_owned();
    vcx.update(|window, cx| input.update(cx, |s, cx| s.set_value(value, window, cx)));
}

fn mock_manager(dir: &std::path::Path) -> SessionManager {
    let mut config = ManagerConfig::with_known_hosts(KnownHostsFiles {
        own: dir.join("known_hosts"),
        user: None,
    });
    config.workers = 1;
    let registry = registry_with(vec![Arc::new(MockDriver::new())]);
    match SessionManager::new(config, registry, SecretStores::new()) {
        Ok(m) => m,
        Err(e) => panic!("{e}"),
    }
}

fn mock_connection(name: &str, ty: ConnectionTypeId, auth: Auth) -> Connection {
    let engine = EngineId::new("mock").unwrap_or_else(|e| panic!("{e}"));
    let mut c = Connection::new(
        name,
        engine,
        Endpoint::Tcp {
            host: "mock.example.test".into(),
            port: Some(5000),
            database: None,
        },
        auth,
        ty,
    );
    // The mock declares no TLS mode (and core refuses a mode outside `Capabilities::tls`).
    c.tls.mode = TlsMode::Disable;
    c
}

fn mock_hub(
    cx: &mut TestAppContext,
    dir: &std::path::Path,
    connections: Vec<Connection>,
) -> Entity<ConnectionHub> {
    let manager = mock_manager(dir);
    let mut config = HubConfig::empty();
    config.registry = manager.registry().clone();
    config.manager = Some(manager);
    config.connections =
        ninesql_core::Connections::from_vec(connections).unwrap_or_else(|e| panic!("{e}"));
    init(cx, config)
}

fn shipped_hub(cx: &mut TestAppContext) -> Entity<ConnectionHub> {
    let mut config = HubConfig::empty();
    config.registry = shipped();
    init(cx, config)
}

#[gpui::test]
fn new_connection_key_opens_the_dialog_on_the_first_engine(cx: &mut TestAppContext) {
    shipped_hub(cx);
    let (ws, mut vcx) = open(cx);
    vcx.simulate_keystrokes("ctrl-alt-n");
    let engine = ws.read_with(&vcx, |ws, cx| {
        ws.connection_dialog()
            .map(|d| d.read(cx).form().engine.clone())
    });
    assert_eq!(engine.as_deref(), Some("postgresql"));
    assert!(vcx.update(|window, cx| window.has_active_dialog(cx)));
    // No type is preselected for a new connection (ADR-0007).
    let ty = ws.read_with(&vcx, |ws, cx| {
        ws.connection_dialog()
            .and_then(|d| d.read(cx).form().connection_type.clone())
    });
    assert!(ty.is_none());
}

#[gpui::test]
fn dialog_saves_into_the_navigator_with_folders(cx: &mut TestAppContext) {
    let hub = shipped_hub(cx);
    let (ws, mut vcx) = open(cx);
    vcx.simulate_keystrokes("ctrl-alt-n");
    let dialog = ws.read_with(&vcx, |ws, _| ws.connection_dialog().cloned());
    let Some(dialog) = dialog else {
        panic!("dialog not open");
    };
    let (name, host, user, folder) = dialog.read_with(&vcx, |d, _| {
        (
            d.input("name").cloned(),
            d.input("host").cloned(),
            d.input("username").cloned(),
            d.input("folder").cloned(),
        )
    });
    set(name.as_ref(), "Sales", &mut vcx);
    set(host.as_ref(), "db.example.test", &mut vcx);
    set(user.as_ref(), "app", &mut vcx);
    set(folder.as_ref(), "team/eu", &mut vcx);
    // Saving without a type is refused.
    vcx.update(|window, cx| dialog.update(cx, |d, cx| d.save(window, cx)));
    vcx.run_until_parked();
    assert!(dialog.read_with(&vcx, |d, _| d.error().is_some()));
    dialog.update(&mut vcx, |d, _| {
        d.form_mut().connection_type = Some(ConnectionTypeId::prod());
    });
    vcx.update(|window, cx| dialog.update(cx, |d, cx| d.save(window, cx)));
    vcx.run_until_parked();
    assert_eq!(hub.read_with(&vcx, |h, _| h.connections().len()), 1);
    assert!(
        ws.read_with(&vcx, |ws, _| ws.connection_dialog().is_none()),
        "closed on save"
    );
    let rows = ws.read_with(&vcx, |ws, cx| ws.navigator().read(cx).rows(cx));
    assert_eq!(
        rows,
        [
            (0, "team/".to_owned()),
            (1, "eu/".to_owned()),
            (2, "Sales".to_owned())
        ]
    );
    let selected = ws.read_with(&vcx, |ws, cx| {
        ws.navigator().read(cx).selected_connection(cx)
    });
    assert!(selected.is_some(), "the saved connection is selected");
    // Production rules apply to the saved connection.
    let c = hub.read_with(&vcx, |h, _| h.connections().iter().next().cloned());
    let Some(c) = c else { panic!("saved") };
    assert!(hub.read_with(&vcx, |h, _| c.policy(h.types()).production));
    assert_eq!(c.tls.mode, TlsMode::VerifyFull, "I5 default");
}

#[gpui::test]
fn properties_off_the_driver_allowlist_are_refused_at_save(cx: &mut TestAppContext) {
    let hub = shipped_hub(cx);
    let (_ws, mut vcx) = open(cx);
    let offer = hub.read_with(&vcx, |h, _| h.offer("postgresql").cloned());
    let Some(offer) = offer else {
        panic!("pg offered")
    };
    let mut form = super::form::ConnectionForm::new("postgresql");
    form.name = "x".into();
    form.host = "h".into();
    form.username = "u".into();
    form.connection_type = Some(ConnectionTypeId::dev());
    form.properties = vec![("sslmode".into(), "disable".into())];
    let built = hub.read_with(&vcx, |h, _| form.build(&offer, vec![], h.types(), None));
    let Ok(built) = built else { panic!("builds") };
    let task = hub.update(&mut vcx, |h, cx| h.save(built, cx));
    vcx.run_until_parked();
    let result = vcx.foreground_executor().block_test(task);
    let Err(e) = result else { panic!("refused") };
    assert!(e.contains("sslmode"), "{e}");
    assert_eq!(hub.read_with(&vcx, |h, _| h.connections().len()), 0);
}

#[gpui::test]
fn saved_password_goes_to_the_store_never_to_connections_json(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let base = tmp.path().to_path_buf();
    let xdg = XdgDirs::from_lookup(|k| {
        let dir = match k {
            "XDG_CONFIG_HOME" => "config",
            "XDG_DATA_HOME" => "data",
            "XDG_STATE_HOME" => "state",
            "XDG_CACHE_HOME" => "cache",
            "HOME" => "home",
            _ => return None,
        };
        Some(base.join(dir).into_os_string())
    })
    .unwrap_or_else(|e| panic!("{e}"));
    let paths = Paths::new(&xdg);
    let workspace = Workspace::open(paths.clone(), ProjectName::default_project())
        .unwrap_or_else(|e| panic!("{e}"));
    let store = Arc::new(MemoryStore::new(ninesql_core::secrets::StoreKind::Vault));
    let mut stores = SecretStores::new();
    stores.register(store.clone());
    let mut config = HubConfig::empty();
    config.registry = shipped();
    config.workspace = Some(workspace);
    config.stores = stores;
    let hub = init(cx, config);
    let (ws, mut vcx) = open(cx);
    vcx.simulate_keystrokes("ctrl-alt-n");
    let Some(dialog) = ws.read_with(&vcx, |ws, _| ws.connection_dialog().cloned()) else {
        panic!("dialog");
    };
    let inputs = dialog.read_with(&vcx, |d, _| {
        ["name", "host", "username", "secret"].map(|f| d.input(f).cloned())
    });
    set(inputs[0].as_ref(), "Vaulted", &mut vcx);
    set(inputs[1].as_ref(), "db.example.test", &mut vcx);
    set(inputs[2].as_ref(), "app", &mut vcx);
    dialog.update(&mut vcx, |d, _| {
        let f = d.form_mut();
        f.connection_type = Some(ConnectionTypeId::dev());
        f.auth_secret.mode = SecretMode::Save;
    });
    set(inputs[3].as_ref(), "s3cr3t-Pa55word", &mut vcx);
    vcx.update(|window, cx| dialog.update(cx, |d, cx| d.save(window, cx)));
    vcx.run_until_parked();
    let saved = hub.read_with(&vcx, |h, _| h.connections().iter().next().cloned());
    let Some(saved) = saved else { panic!("saved") };
    let Auth::Password {
        secret: SecretSource::Stored(reference),
        ..
    } = &saved.auth
    else {
        panic!("password saved by reference");
    };
    assert!(store.len() == 1, "one secret in the store");
    assert!(reference.id.as_str().starts_with(&saved.id.to_string()));
    // I1: no file under the NineSQL directories holds the password.
    for entry in walk(tmp.path()) {
        let bytes = std::fs::read(&entry).unwrap_or_default();
        assert!(
            !String::from_utf8_lossy(&bytes).contains("s3cr3t-Pa55word"),
            "{}",
            entry.display()
        );
    }
    // The masked input was cleared after saving.
    let left = inputs[3]
        .as_ref()
        .map(|i| i.read_with(&vcx, |s, _| s.value().to_string()));
    assert_eq!(left.as_deref(), Some(""));
}

fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(walk(&p));
            } else {
                out.push(p);
            }
        }
    }
    out
}

#[gpui::test]
fn duplicate_and_delete_with_confirmation(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let c = mock_connection("Orders", ConnectionTypeId::dev(), Auth::None);
    let id = c.id;
    let hub = mock_hub(cx, tmp.path(), vec![c]);
    let (ws, mut vcx) = open(cx);
    ws.update(&mut vcx, |ws, cx| {
        ws.navigator()
            .update(cx, |n, cx| n.select_connection(id, cx));
    });
    vcx.simulate_keystrokes("ctrl-k ctrl-d");
    let name = ws.read_with(&vcx, |ws, cx| {
        ws.connection_dialog()
            .map(|d| d.read(cx).form().name.clone())
    });
    assert_eq!(name.as_deref(), Some("Orders (copy)"));
    vcx.update(|window, cx| window.close_dialog(cx));
    vcx.run_until_parked();
    vcx.simulate_keystrokes("ctrl-k delete");
    assert!(
        vcx.update(|window, cx| window.has_active_dialog(cx)),
        "asks first"
    );
    assert_eq!(hub.read_with(&vcx, |h, _| h.connections().len()), 1);
    ws.update_in(&mut vcx, |ws, window, cx| {
        ws.delete_connection_now(id, window, cx)
    });
    vcx.run_until_parked();
    assert_eq!(hub.read_with(&vcx, |h, _| h.connections().len()), 0);
    assert!(ws.read_with(&vcx, |ws, cx| ws.navigator().read(cx).is_empty(cx)));
}

#[gpui::test]
fn needs_secrets_prompts_then_opens_and_session_events_follow(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let c = mock_connection(
        "Asked",
        ConnectionTypeId::dev(),
        Auth::Password {
            username: "app".into(),
            secret: SecretSource::Prompt,
        },
    );
    let id = c.id;
    let hub = mock_hub(cx, tmp.path(), vec![c]);
    let (ws, mut vcx) = open(cx);
    ws.update_in(&mut vcx, |ws, window, cx| {
        ws.connect(id, Purpose::Open, window, cx)
    });
    let flow = ws.read_with(&vcx, |ws, _| ws.flow().clone());
    wait_until(&mut vcx, |vcx| {
        matches!(
            flow.read_with(vcx, |f, _| f.prompt().clone()),
            PendingPrompt::Secrets(_)
        )
    });
    let PendingPrompt::Secrets(view) = flow.read_with(&vcx, |f, _| f.prompt().clone()) else {
        panic!("secret prompt");
    };
    assert_eq!(view.labels.len(), 1);
    assert!(vcx.update(|window, cx| window.has_active_dialog(cx)));
    let inputs = flow.read_with(&vcx, |f, _| f.secret_inputs());
    set(inputs.first(), "typed-once", &mut vcx);
    vcx.update(|window, cx| flow.update(cx, |f, cx| f.submit_secrets(window, cx)));
    wait_until(&mut vcx, |vcx| hub.read_with(vcx, |h, _| h.is_open(id)));
    let last = flow.read_with(&vcx, |f, _| f.last_result().map(|r| r.result.clone()));
    assert!(matches!(last, Some(FlowResult::Opened { .. })), "{last:?}");
    assert_eq!(ws.read_with(&vcx, |ws, _| ws.active_connection()), Some(id));
    assert!(
        ws.read_with(&vcx, |ws, cx| ws.frame_color(cx).is_none()),
        "not production"
    );
    // Disconnect: the core's SessionClosed event clears the session.
    ws.update(&mut vcx, |ws, cx| {
        ws.navigator()
            .update(cx, |n, cx| n.select_connection(id, cx));
    });
    vcx.simulate_keystrokes("ctrl-k ctrl-x");
    wait_until(&mut vcx, |vcx| !hub.read_with(vcx, |h, _| h.is_open(id)));
    assert_eq!(
        hub.read_with(&vcx, |h, _| h.resyncs()),
        0,
        "no event was dropped"
    );
}

#[gpui::test]
fn production_init_sql_is_confirmed_then_framed(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let mut c = mock_connection("Prod", ConnectionTypeId::prod(), Auth::None);
    c.init_sql = vec!["SET search_path TO app".into(), "DELETE FROM audit".into()];
    let id = c.id;
    let hub = mock_hub(cx, tmp.path(), vec![c]);
    let (ws, mut vcx) = open(cx);
    ws.update(&mut vcx, |ws, cx| {
        ws.navigator()
            .update(cx, |n, cx| n.select_connection(id, cx));
    });
    vcx.simulate_keystrokes("ctrl-k ctrl-o");
    let flow = ws.read_with(&vcx, |ws, _| ws.flow().clone());
    wait_until(&mut vcx, |vcx| {
        matches!(
            flow.read_with(vcx, |f, _| f.prompt().clone()),
            PendingPrompt::InitSql(_)
        )
    });
    let PendingPrompt::InitSql(view) = flow.read_with(&vcx, |f, _| f.prompt().clone()) else {
        panic!("init SQL prompt");
    };
    assert!(view.production);
    assert_eq!(view.rows.len(), 2, "every statement is listed");
    assert!(view.flagged() >= 1);
    assert!(view.rows[1].flag.is_some(), "the DELETE is marked");
    vcx.update(|window, cx| flow.update(cx, |f, cx| f.confirm_init_sql(window, cx)));
    wait_until(&mut vcx, |vcx| hub.read_with(vcx, |h, _| h.is_open(id)));
    assert!(
        ws.read_with(&vcx, |ws, cx| ws.frame_color(cx).is_some()),
        "production frame"
    );
}

#[gpui::test]
fn cancelling_a_prompt_opens_nothing(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let c = mock_connection(
        "Asked",
        ConnectionTypeId::dev(),
        Auth::Password {
            username: "app".into(),
            secret: SecretSource::Prompt,
        },
    );
    let id = c.id;
    let hub = mock_hub(cx, tmp.path(), vec![c]);
    let (ws, mut vcx) = open(cx);
    ws.update_in(&mut vcx, |ws, window, cx| {
        ws.connect(id, Purpose::Test, window, cx)
    });
    let flow = ws.read_with(&vcx, |ws, _| ws.flow().clone());
    wait_until(&mut vcx, |vcx| {
        matches!(
            flow.read_with(vcx, |f, _| f.prompt().clone()),
            PendingPrompt::Secrets(_)
        )
    });
    vcx.update(|window, cx| flow.update(cx, |f, cx| f.cancel(window, cx)));
    vcx.run_until_parked();
    let last = flow.read_with(&vcx, |f, _| f.last_result().map(|r| r.result.clone()));
    assert_eq!(last, Some(FlowResult::Cancelled));
    assert!(!hub.read_with(&vcx, |h, _| h.is_open(id)));
    assert!(!flow.read_with(&vcx, |f, _| f.is_busy()));
}
