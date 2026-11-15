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

/// A window for tests on a live driver runtime: its threads wake GPUI tasks (connect replies,
/// core events) at any time, so parking is allowed before the first connect.
pub(crate) fn open_live(cx: &mut TestAppContext) -> (Entity<Shell>, VisualTestContext) {
    let (ws, vcx) = open(cx);
    vcx.executor().allow_parking();
    (ws, vcx)
}

/// Runs the foreground until `done`, letting the driver runtime's threads work in between.
pub(crate) fn wait_until(
    vcx: &mut VisualTestContext,
    mut done: impl FnMut(&mut VisualTestContext) -> bool,
) {
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

pub(crate) fn mock_connection(name: &str, ty: ConnectionTypeId, auth: Auth) -> Connection {
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

pub(crate) fn mock_hub(
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
    let (ws, mut vcx) = open_live(cx);
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
    // The delete runs on the driver runtime's blocking pool.
    wait_until(&mut vcx, |vcx| {
        hub.read_with(vcx, |h, _| h.connections().is_empty())
    });
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
    let (ws, mut vcx) = open_live(cx);
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
    let (ws, mut vcx) = open_live(cx);
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
    let (ws, mut vcx) = open_live(cx);
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

// ---------------------------------------------------------------------------------------------
// Fix round of the P1-C security review.

fn temp_workspace(base: &std::path::Path) -> Workspace {
    let base = base.to_path_buf();
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
    Workspace::open(Paths::new(&xdg), ProjectName::default_project())
        .unwrap_or_else(|e| panic!("{e}"))
}

/// A hub on the shipped drivers, a workspace under `base` and an in-memory vault.
fn vault_hub(
    cx: &mut TestAppContext,
    base: &std::path::Path,
) -> (Entity<ConnectionHub>, Arc<MemoryStore>) {
    let store = Arc::new(MemoryStore::new(ninesql_core::secrets::StoreKind::Vault));
    let mut stores = SecretStores::new();
    stores.register(store.clone());
    let mut config = HubConfig::empty();
    config.registry = shipped();
    config.workspace = Some(temp_workspace(base));
    config.stores = stores;
    (init(cx, config), store)
}

fn run<T: 'static>(cx: &mut TestAppContext, task: gpui::Task<T>) -> T {
    cx.run_until_parked();
    cx.foreground_executor().block_test(task)
}

fn value_of(store: &MemoryStore, r: &ninesql_core::secrets::SecretRef) -> Option<String> {
    use ninesql_core::secrets::{ExposeSecret as _, SecretStore as _};
    store.get(&r.id).ok().map(|s| s.expose_secret().to_owned())
}

fn stored(c: &Connection, slot: ninesql_core::SecretSlot) -> ninesql_core::secrets::SecretRef {
    match c.secret_refs().into_iter().find(|(s, _, _)| *s == slot) {
        Some((_, _, r)) => r.clone(),
        None => panic!("no saved secret at {slot:?}"),
    }
}

fn save_form(
    cx: &mut TestAppContext,
    hub: &Entity<ConnectionHub>,
    form: &super::form::ConnectionForm,
    typed: Vec<(
        ninesql_core::SecretSlot,
        ninesql_core::secrets::SecretString,
    )>,
) -> Result<Connection, String> {
    let built = hub.read_with(cx, |h, _| {
        let Some(offer) = h.offer(&form.engine) else {
            panic!("engine offered");
        };
        form.build(offer, typed, h.types(), h.store_for_new_secrets())
    });
    let built = built.map_err(|e| e.message)?;
    let task = hub.update(cx, |h, cx| h.save(built, cx));
    let id = run(cx, task)?;
    Ok(hub
        .read_with(cx, |h, _| h.connection(id).cloned())
        .unwrap_or_else(|| panic!("saved")))
}

fn secret(s: &str) -> ninesql_core::secrets::SecretString {
    ninesql_core::secrets::SecretString::from(s.to_owned())
}

fn hop(host: &str, auth: super::form::SshAuthChoice) -> super::form::HopForm {
    super::form::HopForm {
        host: host.into(),
        username: "me".into(),
        auth,
        secret: super::form::SecretField {
            mode: SecretMode::Save,
            stored: None,
        },
        ..super::form::HopForm::default()
    }
}

#[gpui::test]
fn removing_and_adding_hops_never_sends_a_password_to_another_host(cx: &mut TestAppContext) {
    use super::form::{ConnectionForm, SshAuthChoice};
    use ninesql_core::SecretSlot;
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let (hub, store) = vault_hub(cx, tmp.path());
    let mut form = ConnectionForm::new("postgresql");
    form.name = "Tunnelled".into();
    form.host = "db.internal.test".into();
    form.username = "app".into();
    form.auth_secret.mode = SecretMode::Ask;
    form.connection_type = Some(ConnectionTypeId::dev());
    form.ssh_enabled = true;
    form.hops = vec![
        hop("jump-a.example.test", SshAuthChoice::Password),
        hop("jump-b.example.test", SshAuthChoice::Password),
        hop("bastion.example.test", SshAuthChoice::Agent),
    ];
    let first = save_form(
        cx,
        &hub,
        &form,
        vec![
            (SecretSlot::SshJump(0), secret("pw-for-a")),
            (SecretSlot::SshJump(1), secret("pw-for-b")),
        ],
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let ref_a = stored(&first, SecretSlot::SshJump(0));
    let ref_b = stored(&first, SecretSlot::SshJump(1));
    assert_ne!(ref_a.id, ref_b.id);
    assert_eq!(store.len(), 2);

    // Remove jump A, add a jump host (inserted before the SSH server, as "Add jump host"
    // does), type its password: jump B moves to index 0, the new host takes index 1.
    let mut edit = ConnectionForm::from_connection(&first);
    edit.hops.remove(0);
    edit.hops
        .insert(1, hop("jump-new.example.test", SshAuthChoice::Password));
    let second = save_form(
        cx,
        &hub,
        &edit,
        vec![(SecretSlot::SshJump(1), secret("pw-for-new"))],
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let ssh = second.ssh.as_ref().unwrap_or_else(|| panic!("ssh"));
    assert_eq!(ssh.jump_hosts[0].host, "jump-b.example.test");
    assert_eq!(ssh.jump_hosts[1].host, "jump-new.example.test");
    let now_b = stored(&second, SecretSlot::SshJump(0));
    let now_new = stored(&second, SecretSlot::SshJump(1));
    assert_eq!(now_b, ref_b, "the moved hop keeps its own secret");
    assert_eq!(value_of(&store, &now_b).as_deref(), Some("pw-for-b"));
    assert_ne!(now_new.id, ref_b.id, "a fresh id, never a positional one");
    assert_eq!(value_of(&store, &now_new).as_deref(), Some("pw-for-new"));
    assert_eq!(
        value_of(&store, &ref_a),
        None,
        "the removed hop's secret is deleted"
    );
    assert_eq!(store.len(), 2);
}

#[gpui::test]
fn deleting_a_connection_deletes_its_saved_secrets(cx: &mut TestAppContext) {
    use super::form::ConnectionForm;
    use ninesql_core::SecretSlot;
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let (hub, store) = vault_hub(cx, tmp.path());
    let mut form = ConnectionForm::new("postgresql");
    form.name = "Gone".into();
    form.host = "db.example.test".into();
    form.username = "app".into();
    form.auth_secret.mode = SecretMode::Save;
    form.connection_type = Some(ConnectionTypeId::dev());
    let c = save_form(cx, &hub, &form, vec![(SecretSlot::Auth, secret("pw"))])
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(store.len(), 1);
    // Switching the field to "ask" deletes the saved secret (M2).
    let mut ask = ConnectionForm::from_connection(&c);
    ask.auth_secret.mode = SecretMode::Ask;
    save_form(cx, &hub, &ask, vec![]).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(store.len(), 0);
    // Saved again, then the connection is deleted with it.
    let mut again = ConnectionForm::from_connection(&c);
    again.auth_secret = super::form::SecretField {
        mode: SecretMode::Save,
        stored: None,
    };
    let c = save_form(cx, &hub, &again, vec![(SecretSlot::Auth, secret("pw2"))])
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(store.len(), 1);
    let task = hub.update(cx, |h, cx| h.remove(c.id, cx));
    run(cx, task).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(store.len(), 0);
    assert_eq!(hub.read_with(cx, |h, _| h.connections().len()), 0);
}

#[gpui::test]
fn a_failed_save_keeps_the_old_secret_and_removes_the_new_one(cx: &mut TestAppContext) {
    use super::form::ConnectionForm;
    use ninesql_core::SecretSlot;
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let (hub, store) = vault_hub(cx, tmp.path());
    let mut form = ConnectionForm::new("postgresql");
    form.name = "Kept".into();
    form.host = "db.example.test".into();
    form.username = "app".into();
    form.auth_secret.mode = SecretMode::Save;
    form.connection_type = Some(ConnectionTypeId::dev());
    let c = save_form(cx, &hub, &form, vec![(SecretSlot::Auth, secret("old-pw"))])
        .unwrap_or_else(|e| panic!("{e}"));
    let old = stored(&c, SecretSlot::Auth);
    // Make the next write of connections.json fail: its directory becomes a file.
    let file = walk(tmp.path())
        .into_iter()
        .find(|p| p.file_name().is_some_and(|n| n == "connections.json"))
        .unwrap_or_else(|| panic!("connections.json written"));
    let dir = file
        .parent()
        .unwrap_or_else(|| panic!("parent"))
        .to_path_buf();
    std::fs::rename(&dir, dir.with_extension("moved")).unwrap_or_else(|e| panic!("{e}"));
    std::fs::write(&dir, b"not a directory").unwrap_or_else(|e| panic!("{e}"));

    let edit = ConnectionForm::from_connection(&c);
    let result = save_form(cx, &hub, &edit, vec![(SecretSlot::Auth, secret("new-pw"))]);
    assert!(result.is_err(), "the write fails");
    assert_eq!(store.len(), 1, "the new secret was rolled back");
    assert_eq!(value_of(&store, &old).as_deref(), Some("old-pw"));
    let kept = hub.read_with(cx, |h, _| h.connection(c.id).cloned());
    let Some(kept) = kept else {
        panic!("still there")
    };
    assert_eq!(
        stored(&kept, SecretSlot::Auth),
        old,
        "the reference was not swapped"
    );
}

#[gpui::test]
fn secret_inputs_are_read_zeroized_and_cleared_on_cancel(cx: &mut TestAppContext) {
    use ninesql_core::secrets::ExposeSecret as _;
    shipped_hub(cx);
    let (ws, mut vcx) = open(cx);
    vcx.simulate_keystrokes("ctrl-alt-n");
    let Some(dialog) = ws.read_with(&vcx, |ws, _| ws.connection_dialog().cloned()) else {
        panic!("dialog");
    };
    let inputs = dialog.read_with(&vcx, |d, _| d.secret_inputs());
    set(inputs.first(), "typed-secret", &mut vcx);
    let read = inputs.first().and_then(|i| {
        vcx.read(|cx| super::secret_input::read_secret(i, cx))
            .map(|s| s.expose_secret().to_owned())
    });
    assert_eq!(read.as_deref(), Some("typed-secret"));
    vcx.update(|window, cx| dialog.update(cx, |d, cx| d.cancel(window, cx)));
    vcx.run_until_parked();
    for input in &inputs {
        assert!(vcx.read(|cx| super::secret_input::is_empty(input, cx)));
    }
    assert!(ws.read_with(&vcx, |ws, _| ws.connection_dialog().is_none()));
    assert!(!vcx.update(|window, cx| window.has_active_dialog(cx)));
}

#[gpui::test]
fn a_dismissed_prompt_cancels_the_run_and_clears_its_input(cx: &mut TestAppContext) {
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
    let (ws, mut vcx) = open_live(cx);
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
    let inputs = flow.read_with(&vcx, |f, _| f.secret_inputs());
    set(inputs.first(), "half-typed", &mut vcx);
    // What the dialog's `on_close` runs on Escape, the close button or a click outside.
    vcx.update(|window, cx| {
        window.close_dialog(cx);
        flow.update(cx, |f, cx| f.abandon(window, cx));
    });
    vcx.run_until_parked();
    assert!(!flow.read_with(&vcx, |f, _| f.is_busy()), "not stuck");
    let last = flow.read_with(&vcx, |f, _| f.last_result().map(|r| r.result.clone()));
    assert_eq!(last, Some(FlowResult::Cancelled));
    assert!(vcx.read(|cx| super::secret_input::is_empty(&inputs[0], cx)));
    assert!(!hub.read_with(&vcx, |h, _| h.is_open(id)));
    // Abandoning again (a late close) does nothing.
    vcx.update(|window, cx| flow.update(cx, |f, cx| f.abandon(window, cx)));
    // A new connect works.
    ws.update_in(&mut vcx, |ws, window, cx| {
        ws.connect(id, Purpose::Open, window, cx)
    });
    wait_until(&mut vcx, |vcx| {
        matches!(
            flow.read_with(vcx, |f, _| f.prompt().clone()),
            PendingPrompt::Secrets(_)
        )
    });
    vcx.update(|window, cx| flow.update(cx, |f, cx| f.cancel(window, cx)));
    vcx.run_until_parked();
    assert!(!flow.read_with(&vcx, |f, _| f.is_busy()));
}

#[gpui::test]
fn the_frame_and_badges_follow_the_open_session(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let c = mock_connection("Live", ConnectionTypeId::prod(), Auth::None);
    let id = c.id;
    let hub = mock_hub(cx, tmp.path(), vec![c.clone()]);
    let (ws, mut vcx) = open_live(cx);
    ws.update_in(&mut vcx, |ws, window, cx| {
        ws.connect(id, Purpose::Open, window, cx)
    });
    wait_until(&mut vcx, |vcx| hub.read_with(vcx, |h, _| h.is_open(id)));
    assert!(ws.read_with(&vcx, |ws, cx| ws.frame_color(cx).is_some()));

    // Saved as Development while connected: the session is still production.
    let mut dev = c.clone();
    dev.connection_type = ConnectionTypeId::dev();
    let task = hub.update(&mut vcx, |h, cx| {
        h.save(
            super::form::BuiltConnection {
                connection: dev,
                to_store: Vec::new(),
                to_delete: Vec::new(),
            },
            cx,
        )
    });
    // The save runs on the driver runtime's blocking pool: wait for it with parking allowed.
    let done = std::rc::Rc::new(std::cell::RefCell::new(None));
    let slot = done.clone();
    vcx.foreground_executor()
        .spawn(async move {
            *slot.borrow_mut() = Some(task.await);
        })
        .detach();
    wait_until(&mut vcx, |_| done.borrow().is_some());
    if let Some(Err(e)) = done.borrow_mut().take() {
        panic!("{e}");
    }
    assert!(
        ws.read_with(&vcx, |ws, cx| ws.frame_color(cx).is_some()),
        "the frame stays until reconnect"
    );
    let b = hub.read_with(&vcx, |h, _| {
        let Some(c) = h.connection(id) else {
            panic!("saved")
        };
        super::visuals::ConnectionBadges::of(c, h.types(), h.sessions(id))
    });
    assert!(b.production && b.pending_reconnect, "{b:?}");
    assert!(b.type_name.contains("until reconnect"), "{}", b.type_name);

    // Editing it now says the changes apply on reconnect.
    ws.update(&mut vcx, |ws, cx| {
        ws.navigator()
            .update(cx, |n, cx| n.select_connection(id, cx));
    });
    vcx.simulate_keystrokes("f4");
    let Some(dialog) = ws.read_with(&vcx, |ws, _| ws.connection_dialog().cloned()) else {
        panic!("dialog");
    };
    let notices = dialog.read_with(&vcx, |d, cx| d.current_notices(cx));
    assert!(
        notices
            .iter()
            .any(|n| n.text.contains("until you reconnect")),
        "{notices:?}"
    );

    // After reconnecting, the saved type applies.
    vcx.update(|window, cx| dialog.update(cx, |d, cx| d.cancel(window, cx)));
    vcx.simulate_keystrokes("ctrl-k ctrl-x");
    wait_until(&mut vcx, |vcx| !hub.read_with(vcx, |h, _| h.is_open(id)));
    ws.update_in(&mut vcx, |ws, window, cx| {
        ws.connect(id, Purpose::Open, window, cx)
    });
    wait_until(&mut vcx, |vcx| hub.read_with(vcx, |h, _| h.is_open(id)));
    assert!(ws.read_with(&vcx, |ws, cx| ws.frame_color(cx).is_none()));
}

// ---------------------------------------------------------------------------------------------
// Integration with the core's `CoreServices` (feat/p1-d-app-wiring).

const MASTER: &str = "correct horse battery staple";

fn temp_paths(base: &std::path::Path) -> Paths {
    let base = base.to_path_buf();
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
    Paths::new(&xdg)
}

fn wait_reply<T>(mut reply: ninesql_core::Reply<T>) -> T {
    for _ in 0..2000 {
        match reply.try_take() {
            ninesql_core::Next::Ready(v) => return v,
            ninesql_core::Next::Pending => std::thread::sleep(Duration::from_millis(5)),
            ninesql_core::Next::Ended => panic!("no answer"),
        }
    }
    panic!("no answer in time");
}

/// A real `CoreServices` on the mock driver under `base` (no Secret Service, no desktop lock
/// watcher, so new secrets go to the vault), installed as the hub.
fn core_hub(
    cx: &mut TestAppContext,
    base: &std::path::Path,
) -> (Entity<ConnectionHub>, ninesql_core::CoreServices) {
    let registry = registry_with(vec![Arc::new(MockDriver::new())]);
    let mut config =
        ninesql_core::CoreConfig::new(temp_paths(base), ProjectName::default_project(), registry);
    config.use_secret_service = false;
    config.watch_desktop_lock = false;
    config.manager.workers = 1;
    let core = ninesql_core::CoreServices::start_blocking(config).unwrap_or_else(|e| panic!("{e}"));
    let hub = init(cx, HubConfig::load_from_core(core.clone()));
    (hub, core)
}

fn create_test_vault(core: &ninesql_core::CoreServices) {
    wait_reply(core.create_vault_with(
        secret(MASTER),
        ninesql_core::secrets::KdfParams::insecure_for_tests(),
    ))
    .unwrap_or_else(|e| panic!("{e}"));
}

/// Awaits a hub task that runs on the driver runtime, with parking allowed.
fn await_task<T: 'static>(vcx: &mut VisualTestContext, task: gpui::Task<T>) -> T {
    let done = std::rc::Rc::new(std::cell::RefCell::new(None));
    let slot = done.clone();
    vcx.foreground_executor()
        .spawn(async move {
            *slot.borrow_mut() = Some(task.await);
        })
        .detach();
    wait_until(vcx, |_| done.borrow().is_some());
    let value = done.borrow_mut().take();
    value.unwrap_or_else(|| panic!("done"))
}

fn vaulted_mock(vcx: &mut VisualTestContext, hub: &Entity<ConnectionHub>) -> ConnectionId {
    let c = mock_connection(
        "Vaulted",
        ConnectionTypeId::dev(),
        Auth::Password {
            username: "app".into(),
            secret: SecretSource::Prompt,
        },
    );
    let task = hub.update(vcx, |h, cx| {
        h.save(
            super::form::BuiltConnection {
                connection: c,
                to_store: vec![(ninesql_core::SecretSlot::Auth, secret("db-password"))],
                to_delete: Vec::new(),
            },
            cx,
        )
    });
    await_task(vcx, task).unwrap_or_else(|e| panic!("{e}"))
}

use ninesql_core::ConnectionId;

#[gpui::test]
fn the_hub_uses_the_core_stores_and_backend(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let (hub, core) = core_hub(cx, tmp.path());
    assert_eq!(core.secret_backend(), ninesql_core::SecretBackend::Vault);
    assert_eq!(
        hub.read_with(cx, |h, _| h.store_for_new_secrets()),
        Some(ninesql_core::secrets::StoreKind::Vault)
    );
    // The watcher is off in this test: the problems dialog says so.
    let problems = hub.read_with(cx, |h, _| h.problems());
    assert!(
        problems.iter().any(|p| p.contains("Lock secrets now")),
        "{problems:?}"
    );
}

#[gpui::test]
fn needs_vault_unlock_prompts_unlocks_and_reopens(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let (hub, core) = core_hub(cx, tmp.path());
    create_test_vault(&core);
    let (ws, mut vcx) = open_live(cx);
    let id = vaulted_mock(&mut vcx, &hub);
    let saved = hub.read_with(&vcx, |h, _| h.connection(id).cloned());
    let Some(saved) = saved else { panic!("saved") };
    let r = stored(&saved, ninesql_core::SecretSlot::Auth);
    assert_eq!(r.store, ninesql_core::secrets::StoreKind::Vault);

    // Lock, then connect: the core answers NeedsVaultUnlock.
    wait_reply(core.lock_now());
    wait_until(&mut vcx, |vcx| {
        hub.read_with(vcx, |h, _| h.secrets_locked().is_some())
    });
    ws.update_in(&mut vcx, |ws, window, cx| {
        ws.connect(id, Purpose::Open, window, cx)
    });
    let flow = ws.read_with(&vcx, |ws, _| ws.flow().clone());
    wait_until(&mut vcx, |vcx| {
        flow.read_with(vcx, |f, _| f.prompt().clone()) == PendingPrompt::VaultUnlock
    });
    let Some(prompt) = flow.read_with(&vcx, |f, _| f.vault_prompt()) else {
        panic!("vault prompt");
    };
    assert_eq!(
        prompt.read_with(&vcx, |p, _| p.mode()),
        super::vault::VaultMode::Unlock
    );
    assert!(vcx.update(|window, cx| window.has_active_dialog(cx)));
    let (master, _) = prompt.read_with(&vcx, |p, _| p.inputs());
    set(Some(&master), MASTER, &mut vcx);
    vcx.update(|window, cx| prompt.update(cx, |p, cx| p.submit(window, cx)));
    assert!(
        vcx.read(|cx| super::secret_input::is_empty(&master, cx)),
        "cleared at once"
    );
    // Unlocked, then opened again with the saved password.
    wait_until(&mut vcx, |vcx| hub.read_with(vcx, |h, _| h.is_open(id)));
    let last = flow.read_with(&vcx, |f, _| f.last_result().map(|r| r.result.clone()));
    assert!(matches!(last, Some(FlowResult::Opened { .. })), "{last:?}");
    assert_eq!(hub.read_with(&vcx, |h, _| h.secrets_locked()), None);
}

#[gpui::test]
fn cancelling_the_vault_prompt_cancels_the_open(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let (hub, core) = core_hub(cx, tmp.path());
    create_test_vault(&core);
    let (ws, mut vcx) = open_live(cx);
    let id = vaulted_mock(&mut vcx, &hub);
    wait_reply(core.lock_now());
    ws.update_in(&mut vcx, |ws, window, cx| {
        ws.connect(id, Purpose::Open, window, cx)
    });
    let flow = ws.read_with(&vcx, |ws, _| ws.flow().clone());
    wait_until(&mut vcx, |vcx| {
        flow.read_with(vcx, |f, _| f.vault_prompt().is_some())
    });
    let Some(prompt) = flow.read_with(&vcx, |f, _| f.vault_prompt()) else {
        panic!("vault prompt");
    };
    let (master, _) = prompt.read_with(&vcx, |p, _| p.inputs());
    set(Some(&master), "half", &mut vcx);
    // What Escape / a click outside runs.
    vcx.update(|window, cx| {
        window.close_dialog(cx);
        prompt.update(cx, |p, cx| p.dismissed(window, cx));
    });
    vcx.run_until_parked();
    assert!(vcx.read(|cx| super::secret_input::is_empty(&master, cx)));
    assert!(!flow.read_with(&vcx, |f, _| f.is_busy()));
    let last = flow.read_with(&vcx, |f, _| f.last_result().map(|r| r.result.clone()));
    assert_eq!(last, Some(FlowResult::Cancelled));
    assert!(!hub.read_with(&vcx, |h, _| h.is_open(id)));
}

#[gpui::test]
fn lock_secrets_action_shows_the_lock_notice(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let (hub, core) = core_hub(cx, tmp.path());
    create_test_vault(&core);
    let (_ws, mut vcx) = open_live(cx);
    vcx.update(|window, cx| window.clear_notifications(cx));
    vcx.simulate_keystrokes("ctrl-alt-l");
    wait_until(&mut vcx, |vcx| {
        hub.read_with(vcx, |h, _| h.secrets_locked().is_some())
    });
    assert_eq!(
        hub.read_with(&vcx, |h, _| h.secrets_locked()),
        Some(ninesql_core::SecretsLockReason::Requested)
    );
    vcx.run_until_parked();
    let notes = vcx.update(|window, cx| window.notifications(cx).len());
    assert!(notes >= 1, "the lock notice is shown");
    let status = wait_reply(core.vault_status()).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(status, ninesql_core::secrets::VaultStatus::Locked);
    assert_eq!(
        super::hub::lock_reason_text(ninesql_core::SecretsLockReason::Sleep),
        "the computer went to sleep"
    );
}

#[gpui::test]
fn saving_a_password_to_a_locked_vault_unlocks_it_first(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let (hub, core) = core_hub(cx, tmp.path());
    create_test_vault(&core);
    wait_reply(core.lock_now());
    let c = mock_connection(
        "Edited",
        ConnectionTypeId::dev(),
        Auth::Password {
            username: "app".into(),
            secret: SecretSource::Prompt,
        },
    );
    let (ws, mut vcx) = open_live(cx);
    let mut form = super::form::ConnectionForm::from_connection(&c);
    form.existing = None;
    form.auth_secret.mode = SecretMode::Save;
    ws.update_in(&mut vcx, |ws, window, cx| {
        ws.open_connection_dialog(form, "New connection", window, cx);
    });
    let Some(dialog) = ws.read_with(&vcx, |ws, _| ws.connection_dialog().cloned()) else {
        panic!("dialog");
    };
    let secret_input = dialog.read_with(&vcx, |d, _| d.input("secret").cloned());
    set(secret_input.as_ref(), "db-password", &mut vcx);
    vcx.update(|window, cx| dialog.update(cx, |d, cx| d.save(window, cx)));
    // The vault is locked: the master-password prompt opens instead of a failed save.
    wait_until(&mut vcx, |vcx| {
        dialog.read_with(vcx, |d, _| d.vault_prompt().is_some())
    });
    assert_eq!(hub.read_with(&vcx, |h, _| h.connections().len()), 0);
    let Some(prompt) = dialog.read_with(&vcx, |d, _| d.vault_prompt().cloned()) else {
        panic!("vault prompt");
    };
    assert_eq!(
        prompt.read_with(&vcx, |p, _| p.mode()),
        super::vault::VaultMode::Unlock
    );
    let (master, _) = prompt.read_with(&vcx, |p, _| p.inputs());
    set(Some(&master), MASTER, &mut vcx);
    vcx.update(|window, cx| prompt.update(cx, |p, cx| p.submit(window, cx)));
    // Unlocked, then saved, with the password in the vault.
    wait_until(&mut vcx, |vcx| {
        hub.read_with(vcx, |h, _| h.connections().len() == 1)
    });
    let saved = hub.read_with(&vcx, |h, _| h.connections().iter().next().cloned());
    let Some(saved) = saved else { panic!("saved") };
    let r = stored(&saved, ninesql_core::SecretSlot::Auth);
    assert_eq!(r.store, ninesql_core::secrets::StoreKind::Vault);
    assert!(secret_input.is_some_and(|i| vcx.read(|cx| super::secret_input::is_empty(&i, cx))));
}

#[gpui::test]
fn the_scram_only_toggle_follows_the_driver(cx: &mut TestAppContext) {
    let hub = shipped_hub(cx);
    let (ws, mut vcx) = open(cx);
    vcx.simulate_keystrokes("ctrl-alt-n");
    let Some(dialog) = ws.read_with(&vcx, |ws, _| ws.connection_dialog().cloned()) else {
        panic!("dialog");
    };
    // PostgreSQL (pgwire enforces ScramOnly): shown.
    assert!(dialog.read_with(&vcx, |d, cx| d.scram_only_shown(cx)));
    let inputs = dialog.read_with(&vcx, |d, _| {
        ["name", "host", "username"].map(|f| d.input(f).cloned())
    });
    set(inputs[0].as_ref(), "Scram", &mut vcx);
    set(inputs[1].as_ref(), "db.example.test", &mut vcx);
    set(inputs[2].as_ref(), "app", &mut vcx);
    dialog.update(&mut vcx, |d, cx| {
        d.form_mut().connection_type = Some(ConnectionTypeId::dev());
        d.toggle_scram_only(cx);
    });
    // Below verify-ca the warning appears; at verify-full it does not.
    dialog.update(&mut vcx, |d, _| d.form_mut().tls_mode = TlsMode::Require);
    let notices = dialog.read_with(&vcx, |d, cx| d.current_notices(cx));
    assert!(
        notices.iter().any(|n| n.text.contains("SCRAM only")),
        "{notices:?}"
    );
    dialog.update(&mut vcx, |d, _| d.form_mut().tls_mode = TlsMode::VerifyFull);
    let notices = dialog.read_with(&vcx, |d, cx| d.current_notices(cx));
    assert!(!notices.iter().any(|n| n.text.contains("SCRAM only")));
    vcx.update(|window, cx| dialog.update(cx, |d, cx| d.save(window, cx)));
    vcx.run_until_parked();
    let saved = hub.read_with(&vcx, |h, _| h.connections().iter().next().cloned());
    let Some(saved) = saved else { panic!("saved") };
    assert_eq!(
        saved.password_exchange,
        ninesql_core::PasswordExchange::ScramOnly
    );
    // MySQL (mysqlwire refuses ScramOnly): hidden, and the toggle does nothing.
    vcx.simulate_keystrokes("ctrl-alt-n");
    let Some(dialog) = ws.read_with(&vcx, |ws, _| ws.connection_dialog().cloned()) else {
        panic!("dialog");
    };
    dialog.update(&mut vcx, |d, cx| d.set_engine("mysql", cx));
    assert!(!dialog.read_with(&vcx, |d, cx| d.scram_only_shown(cx)));
    dialog.update(&mut vcx, |d, cx| d.toggle_scram_only(cx));
    assert!(!dialog.read_with(&vcx, |d, _| d.form().scram_only));
}

// ---------------------------------------------------------------------------------------------
// Re-review follow-ups (L-A, L-B, L-C).

#[gpui::test]
fn a_vault_prompt_dismissed_during_the_unlock_does_not_save(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let (hub, core) = core_hub(cx, tmp.path());
    create_test_vault(&core);
    wait_reply(core.lock_now());
    let c = mock_connection(
        "Dismissed",
        ConnectionTypeId::dev(),
        Auth::Password {
            username: "app".into(),
            secret: SecretSource::Prompt,
        },
    );
    let (ws, mut vcx) = open_live(cx);
    let mut form = super::form::ConnectionForm::from_connection(&c);
    form.existing = None;
    form.auth_secret.mode = SecretMode::Save;
    ws.update_in(&mut vcx, |ws, window, cx| {
        ws.open_connection_dialog(form, "New connection", window, cx);
    });
    let Some(dialog) = ws.read_with(&vcx, |ws, _| ws.connection_dialog().cloned()) else {
        panic!("dialog");
    };
    let secret_input = dialog.read_with(&vcx, |d, _| d.input("secret").cloned());
    set(secret_input.as_ref(), "db-password", &mut vcx);
    vcx.update(|window, cx| dialog.update(cx, |d, cx| d.save(window, cx)));
    wait_until(&mut vcx, |vcx| {
        dialog.read_with(vcx, |d, _| d.vault_prompt().is_some())
    });
    let Some(prompt) = dialog.read_with(&vcx, |d, _| d.vault_prompt().cloned()) else {
        panic!("vault prompt");
    };
    let (master, _) = prompt.read_with(&vcx, |p, _| p.inputs());
    set(Some(&master), MASTER, &mut vcx);
    // Submit, then Escape before the key derivation answers.
    vcx.update(|window, cx| {
        prompt.update(cx, |p, cx| p.submit(window, cx));
        window.close_dialog(cx);
        prompt.update(cx, |p, cx| p.dismissed(window, cx));
    });
    // The unlock still completes in the core...
    wait_until(&mut vcx, |_| {
        wait_reply(core.vault_status()).ok() == Some(ninesql_core::secrets::VaultStatus::Unlocked)
    });
    wait_until(&mut vcx, |vcx| {
        hub.read_with(vcx, |h, _| h.secrets_locked().is_none())
    });
    vcx.run_until_parked();
    // ...but nothing is saved and the connection dialog is still open.
    assert_eq!(hub.read_with(&vcx, |h, _| h.connections().len()), 0);
    assert!(ws.read_with(&vcx, |ws, _| ws.connection_dialog().is_some()));
    assert!(
        vcx.update(|window, cx| window.has_active_dialog(cx)),
        "not closed by the late answer"
    );
    let error = dialog.read_with(&vcx, |d, _| d.error().map(str::to_owned));
    assert!(error.is_some_and(|e| e.contains("Not saved")));
}

#[gpui::test]
fn a_create_that_loses_a_lock_race_switches_to_unlock(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let (hub, _core) = core_hub(cx, tmp.path());
    let (_ws, mut vcx) = open_live(cx);
    let prompt = vcx.update(|window, cx| {
        super::vault::open_vault_prompt(hub.clone(), super::vault::VaultMode::Create, window, cx)
    });
    vcx.update(|window, cx| {
        prompt.update(cx, |p, cx| {
            p.apply_result(
                Some(Err(ninesql_core::CoreError::Vault(
                    ninesql_core::secrets::VaultError::Locked,
                ))),
                window,
                cx,
            );
        });
    });
    assert_eq!(
        prompt.read_with(&vcx, |p, _| p.mode()),
        super::vault::VaultMode::Unlock
    );
    let error = prompt.read_with(&vcx, |p, _| p.error().map(str::to_owned));
    assert!(error.is_some_and(|e| e.contains("unlock")));
    assert!(
        vcx.update(|window, cx| window.has_active_dialog(cx)),
        "stays open"
    );
}

#[gpui::test]
fn invalid_saved_connections_are_listed_as_problems(cx: &mut TestAppContext) {
    let engine = EngineId::new("postgresql").unwrap_or_else(|e| panic!("{e}"));
    let mut bad = Connection::new(
        "Broken",
        engine,
        Endpoint::Tcp {
            host: "db.example.test".into(),
            port: None,
            database: None,
        },
        Auth::Password {
            username: "app".into(),
            secret: SecretSource::Prompt,
        },
        ConnectionTypeId::dev(),
    );
    bad.properties
        .insert("not_on_the_allowlist".into(), "x".into());
    let mut config = HubConfig::empty();
    config.registry = shipped();
    config.connections =
        ninesql_core::Connections::from_vec(vec![bad]).unwrap_or_else(|e| panic!("{e}"));
    let hub = init(cx, config);
    let problems = hub.read_with(cx, |h, _| h.problems());
    assert!(
        problems
            .iter()
            .any(|p| p.contains("\"Broken\"") && p.contains("no connection can be saved")),
        "{problems:?}"
    );
}

// ---------------------------------------------------------------------------------------------
// P1-E review, L4: the master password rule when the vault is created.

#[gpui::test]
fn vault_creation_shows_and_enforces_the_master_password_rule(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let (hub, core) = core_hub(cx, tmp.path());
    let (_ws, mut vcx) = open_live(cx);
    let prompt = vcx.update(|window, cx| {
        super::vault::open_vault_prompt(hub.clone(), super::vault::VaultMode::Create, window, cx)
    });
    vcx.run_until_parked();
    assert_eq!(
        super::vault::master_password_rule(),
        "The master password must be at least 12 characters long."
    );
    // Too short (11 characters, more bytes): refused before the key derivation, inputs cleared.
    let (master, confirm) = prompt.read_with(&vcx, |p, _| p.inputs());
    set(Some(&master), "ééééééééééé", &mut vcx);
    set(Some(&confirm), "ééééééééééé", &mut vcx);
    vcx.update(|window, cx| prompt.update(cx, |p, cx| p.submit(window, cx)));
    let error = prompt.read_with(&vcx, |p, _| p.error().map(str::to_owned));
    assert_eq!(error, Some(super::vault::master_password_rule()));
    assert!(!prompt.read_with(&vcx, |p, _| p.is_busy()));
    assert!(vcx.read(|cx| super::secret_input::is_empty(&master, cx)));
    assert!(matches!(
        wait_reply(core.vault_status()),
        Ok(ninesql_core::secrets::VaultStatus::NotCreated)
    ));
    // The core's own refusal reads the same way.
    let text = super::vault::vault_error_text(&ninesql_core::CoreError::Vault(
        ninesql_core::secrets::VaultError::PasswordTooShort { min: 12 },
    ));
    assert_eq!(text, super::vault::master_password_rule());

    // Long enough: created and unlocked.
    set(Some(&master), MASTER, &mut vcx);
    set(Some(&confirm), MASTER, &mut vcx);
    vcx.update(|window, cx| prompt.update(cx, |p, cx| p.submit(window, cx)));
    wait_until(&mut vcx, |_| {
        matches!(
            wait_reply(core.vault_status()),
            Ok(ninesql_core::secrets::VaultStatus::Unlocked)
        )
    });
    assert_eq!(
        prompt.read_with(&vcx, |p, _| p.error().map(str::to_owned)),
        None
    );
}

// ---------------------------------------------------------------------------------------------
// P1-E review, L6 (I1): the hub flows leave no secret in any file nor in any log event.

/// Every log event of the process, at every level (the global subscriber: the driver runtime's
/// threads are captured too). Other tests of this binary log into it as well; they never use the
/// secrets of the L6 test.
#[derive(Clone, Default)]
struct LogCapture(Arc<std::sync::Mutex<String>>);

impl LogCapture {
    fn text(&self) -> String {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn buf(&self) -> std::sync::MutexGuard<'_, String> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

struct LogVisitor<'a>(&'a mut String);

impl tracing::field::Visit for LogVisitor<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        use std::fmt::Write as _;
        let _ = write!(self.0, " {}={:?}", field.name(), value);
    }
}

impl tracing::Subscriber for LogCapture {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        span.record(&mut LogVisitor(&mut self.buf()));
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, values: &tracing::span::Record<'_>) {
        values.record(&mut LogVisitor(&mut self.buf()));
    }
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        let mut buf = self.buf();
        buf.push('\n');
        buf.push_str(event.metadata().target());
        event.record(&mut LogVisitor(&mut buf));
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

fn log_capture() -> &'static LogCapture {
    static CAPTURE: std::sync::OnceLock<LogCapture> = std::sync::OnceLock::new();
    CAPTURE.get_or_init(|| {
        let capture = LogCapture::default();
        if tracing::subscriber::set_global_default(capture.clone()).is_err() {
            panic!("another global subscriber is installed in this test binary");
        }
        capture
    })
}

/// Every file under `dir`; a directory or entry that cannot be read fails the test (an I1 scan
/// must not skip what it cannot see).
fn walk_strict(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let entries =
        std::fs::read_dir(dir).unwrap_or_else(|e| panic!("{} cannot be read: {e}", dir.display()));
    for entry in entries {
        let path = entry
            .unwrap_or_else(|e| panic!("{} cannot be listed: {e}", dir.display()))
            .path();
        if path.is_dir() {
            out.extend(walk_strict(&path));
        } else {
            out.push(path);
        }
    }
    out
}

/// The flow's last result.
fn last_flow(
    vcx: &mut VisualTestContext,
    flow: &Entity<super::connect_flow::ConnectFlow>,
) -> Option<super::connect_flow::FlowFinished> {
    flow.read_with(vcx, |f, _| f.last_result().cloned())
}

/// The result of the flow's next run: the first finished result that differs from `before`.
fn wait_flow_result(
    vcx: &mut VisualTestContext,
    flow: &Entity<super::connect_flow::ConnectFlow>,
    before: Option<super::connect_flow::FlowFinished>,
) -> FlowResult {
    let mut out = None;
    wait_until(vcx, |vcx| {
        let last = last_flow(vcx, flow);
        if last != before && !flow.read_with(vcx, |f, _| f.is_busy()) {
            out = last.map(|r| r.result);
        }
        out.is_some()
    });
    out.unwrap_or_else(|| panic!("no result"))
}

#[gpui::test]
fn hub_flows_leave_no_secret_in_files_or_logs(cx: &mut TestAppContext) {
    const DB_PASSWORD: &str = "L6-db-Pa55-7f3a91c2";
    const DB_PASSWORD_EDITED: &str = "L6-db-Pa55-edited-5b8e";
    const DB_PASSWORD_COPY: &str = "L6-db-Pa55-copy-0d4c";
    const DB_PASSWORD_PROMPTED: &str = "L6-db-Pa55-asked-91e6";
    const L6_MASTER: &str = "L6-master-Pa55-e2a7b9";
    let secrets = [
        DB_PASSWORD,
        DB_PASSWORD_EDITED,
        DB_PASSWORD_COPY,
        DB_PASSWORD_PROMPTED,
        L6_MASTER,
    ];

    let capture = log_capture();
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let (hub, core) = core_hub(cx, tmp.path());
    let (ws, mut vcx) = open_live(cx);
    let flow = ws.read_with(&vcx, |ws, _| ws.flow().clone());

    // Vault create, through the prompt (the default key derivation parameters).
    let prompt = vcx.update(|window, cx| {
        super::vault::open_vault_prompt(hub.clone(), super::vault::VaultMode::Create, window, cx)
    });
    let (master, confirm) = prompt.read_with(&vcx, |p, _| p.inputs());
    set(Some(&master), L6_MASTER, &mut vcx);
    set(Some(&confirm), L6_MASTER, &mut vcx);
    vcx.update(|window, cx| prompt.update(cx, |p, cx| p.submit(window, cx)));
    wait_until(&mut vcx, |_| {
        matches!(
            wait_reply(core.vault_status()),
            Ok(ninesql_core::secrets::VaultStatus::Unlocked)
        )
    });

    // Create through `hub.save`.
    let created = mock_connection(
        "Ledger",
        ConnectionTypeId::dev(),
        Auth::Password {
            username: "app".into(),
            secret: SecretSource::Prompt,
        },
    );
    let task = hub.update(&mut vcx, |h, cx| {
        h.save(
            super::form::BuiltConnection {
                connection: created,
                to_store: vec![(ninesql_core::SecretSlot::Auth, secret(DB_PASSWORD))],
                to_delete: Vec::new(),
            },
            cx,
        )
    });
    let id = await_task(&mut vcx, task).unwrap_or_else(|e| panic!("{e}"));

    // Edit through the dialog's form: renamed, new password.
    let saved = hub
        .read_with(&vcx, |h, _| h.connection(id).cloned())
        .unwrap_or_else(|| panic!("saved"));
    let mut form = super::form::ConnectionForm::from_connection(&saved);
    form.name = "Ledger (edited)".into();
    let built = hub.read_with(&vcx, |h, _| {
        let offer = h.offer("mock").unwrap_or_else(|| panic!("mock offered"));
        form.build(
            offer,
            vec![(ninesql_core::SecretSlot::Auth, secret(DB_PASSWORD_EDITED))],
            h.types(),
            h.store_for_new_secrets(),
        )
    });
    let built = built.unwrap_or_else(|e| panic!("{}", e.message));
    let task = hub.update(&mut vcx, |h, cx| h.save(built, cx));
    assert_eq!(await_task(&mut vcx, task), Ok(id));

    // Duplicate: a copy with its own password.
    let edited = hub
        .read_with(&vcx, |h, _| h.connection(id).cloned())
        .unwrap_or_else(|| panic!("edited"));
    let mut form = super::form::ConnectionForm::duplicate_of(&edited);
    // Saved credentials are not copied: the user types the copy's password and saves it.
    form.auth_secret.mode = SecretMode::Save;
    let built = hub.read_with(&vcx, |h, _| {
        let offer = h.offer("mock").unwrap_or_else(|| panic!("mock offered"));
        form.build(
            offer,
            vec![(ninesql_core::SecretSlot::Auth, secret(DB_PASSWORD_COPY))],
            h.types(),
            h.store_for_new_secrets(),
        )
    });
    let built = built.unwrap_or_else(|e| panic!("{}", e.message));
    let task = hub.update(&mut vcx, |h, cx| h.save(built, cx));
    let copy = await_task(&mut vcx, task).unwrap_or_else(|e| panic!("{e}"));
    assert_ne!(copy, id);
    assert_eq!(hub.read_with(&vcx, |h, _| h.connections().len()), 2);

    // Test connection, with the saved (edited) password.
    let before = last_flow(&mut vcx, &flow);
    ws.update_in(&mut vcx, |ws, window, cx| {
        ws.connect(id, Purpose::Test, window, cx)
    });
    let result = wait_flow_result(&mut vcx, &flow, before);
    assert!(matches!(result, FlowResult::TestOk { .. }), "{result:?}");

    // An "ask every time" password, typed at connect.
    let asked = mock_connection(
        "Asked",
        ConnectionTypeId::dev(),
        Auth::Password {
            username: "app".into(),
            secret: SecretSource::Prompt,
        },
    );
    let task = hub.update(&mut vcx, |h, cx| {
        h.save(
            super::form::BuiltConnection {
                connection: asked,
                to_store: Vec::new(),
                to_delete: Vec::new(),
            },
            cx,
        )
    });
    let asked = await_task(&mut vcx, task).unwrap_or_else(|e| panic!("{e}"));
    let before = last_flow(&mut vcx, &flow);
    ws.update_in(&mut vcx, |ws, window, cx| {
        ws.connect(asked, Purpose::Open, window, cx)
    });
    wait_until(&mut vcx, |vcx| {
        matches!(
            flow.read_with(vcx, |f, _| f.prompt().clone()),
            PendingPrompt::Secrets(_)
        )
    });
    let inputs = flow.read_with(&vcx, |f, _| f.secret_inputs());
    set(inputs.first(), DB_PASSWORD_PROMPTED, &mut vcx);
    vcx.update(|window, cx| flow.update(cx, |f, cx| f.submit_secrets(window, cx)));
    let result = wait_flow_result(&mut vcx, &flow, before);
    assert!(matches!(result, FlowResult::Opened { .. }), "{result:?}");
    hub.update(&mut vcx, |h, cx| h.disconnect(asked, cx));
    wait_until(&mut vcx, |vcx| hub.read_with(vcx, |h, _| !h.is_open(asked)));

    // Lock, then open: the vault unlock prompt, then the session.
    wait_reply(core.lock_now());
    wait_until(&mut vcx, |vcx| {
        hub.read_with(vcx, |h, _| h.secrets_locked().is_some())
    });
    let before = last_flow(&mut vcx, &flow);
    ws.update_in(&mut vcx, |ws, window, cx| {
        ws.connect(copy, Purpose::Open, window, cx)
    });
    wait_until(&mut vcx, |vcx| {
        flow.read_with(vcx, |f, _| f.vault_prompt().is_some())
    });
    let prompt = flow
        .read_with(&vcx, |f, _| f.vault_prompt())
        .unwrap_or_else(|| panic!("vault prompt"));
    let (master, _) = prompt.read_with(&vcx, |p, _| p.inputs());
    set(Some(&master), L6_MASTER, &mut vcx);
    vcx.update(|window, cx| prompt.update(cx, |p, cx| p.submit(window, cx)));
    let result = wait_flow_result(&mut vcx, &flow, before);
    assert!(matches!(result, FlowResult::Opened { .. }), "{result:?}");
    hub.update(&mut vcx, |h, cx| h.disconnect(copy, cx));
    wait_reply(core.lock_now());
    wait_until(&mut vcx, |vcx| hub.read_with(vcx, |h, _| !h.is_open(copy)));
    wait_until(&mut vcx, |vcx| {
        hub.read_with(vcx, |h, _| h.secrets_locked().is_some())
    });

    // No file under the workspace, config, data, state, cache or home directories holds a
    // secret (the vault file is encrypted).
    let files = walk_strict(tmp.path());
    assert!(
        files.iter().any(|f| f.ends_with("vault.bin")),
        "the vault was written: {files:?}"
    );
    assert!(
        files.iter().any(|f| f.ends_with("connections.json")),
        "connections were written: {files:?}"
    );
    for file in &files {
        let bytes = std::fs::read(file)
            .unwrap_or_else(|e| panic!("{} cannot be read: {e}", file.display()));
        let text = String::from_utf8_lossy(&bytes);
        for s in secrets {
            assert!(!text.contains(s), "{} holds a secret", file.display());
        }
    }
    // Nor any log event, at any level, on any thread. The capture saw the flow.
    let logs = capture.text();
    assert!(logs.contains("vault created"), "the capture is live");
    assert!(logs.contains("session opened"), "the capture is live");
    for s in secrets {
        assert!(!logs.contains(s), "a log event holds a secret");
    }
}

// ---------------------------------------------------------------------------------------------
// Imported init SQL marker (`Connection::init_sql_needs_confirmation`).

#[gpui::test]
fn imported_init_sql_is_flagged_confirmed_and_cleared_in_the_hub(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    // A dev type confirms nothing by itself: only the marker asks.
    let mut c = mock_connection("Imported", ConnectionTypeId::dev(), Auth::None);
    c.init_sql = vec!["SET search_path TO app".into()];
    c.init_sql_needs_confirmation = true;
    let id = c.id;
    let hub = mock_hub(cx, tmp.path(), vec![c]);
    assert!(hub.read_with(cx, |h, _| h.init_sql_marker(id)));
    let (ws, mut vcx) = open_live(cx);
    ws.update_in(&mut vcx, |ws, window, cx| {
        ws.connect(id, Purpose::Open, window, cx)
    });
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
    assert!(view.imported, "the dialog shows the imported notice");
    assert!(!view.production);
    let flag = view.rows[0].flag.clone().unwrap_or_default();
    assert!(flag.contains("imported and not confirmed yet"), "{flag}");
    assert!(!flag.contains("connection type"), "{flag}");
    vcx.update(|window, cx| flow.update(cx, |f, cx| f.confirm_init_sql(window, cx)));
    wait_until(&mut vcx, |vcx| {
        hub.read_with(vcx, |h, _| h.is_open(id) && !h.init_sql_marker(id))
    });
    let copy = hub.read_with(&vcx, |h, _| h.connection(id).cloned());
    assert_eq!(
        copy.map(|c| c.init_sql_needs_confirmation),
        Some(false),
        "the hub's copy follows ImportedInitSqlConfirmed"
    );
}

fn imported_pg(name: &str) -> Connection {
    let engine = EngineId::new("postgresql").unwrap_or_else(|e| panic!("{e}"));
    let mut c = Connection::new(
        name,
        engine,
        Endpoint::Tcp {
            host: "db.example.test".into(),
            port: None,
            database: None,
        },
        Auth::Username {
            username: "app".into(),
        },
        ConnectionTypeId::dev(),
    );
    c.init_sql = vec!["SET search_path TO app".into()];
    c
}

#[gpui::test]
fn edits_keep_the_imported_marker_and_never_set_it(cx: &mut TestAppContext) {
    use super::form::ConnectionForm;

    let marked = {
        let mut c = imported_pg("Imported");
        c.init_sql_needs_confirmation = true;
        c
    };
    let plain = imported_pg("Plain");
    let (marked_id, plain_id) = (marked.id, plain.id);
    let mut config = HubConfig::empty();
    config.registry = shipped();
    config.connections = ninesql_core::Connections::from_vec(vec![marked.clone(), plain.clone()])
        .unwrap_or_else(|e| panic!("{e}"));
    let hub = init(cx, config);

    // An edit keeps the marker and the init SQL.
    let mut edit = ConnectionForm::from_connection(&marked);
    edit.name = "Imported, renamed".into();
    let saved = save_form(cx, &hub, &edit, vec![]).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(saved.name, "Imported, renamed");
    assert!(
        saved.init_sql_needs_confirmation,
        "an edit keeps the marker"
    );
    assert_eq!(saved.init_sql, marked.init_sql);

    // An edit never sets it; neither does a duplicate.
    let mut edit = ConnectionForm::from_connection(&plain);
    edit.name = "Plain, renamed".into();
    let saved = save_form(cx, &hub, &edit, vec![]).unwrap_or_else(|e| panic!("{e}"));
    assert!(!saved.init_sql_needs_confirmation);
    let copy = save_form(cx, &hub, &ConnectionForm::duplicate_of(&marked), vec![])
        .unwrap_or_else(|e| panic!("{e}"));
    assert_ne!(copy.id, marked_id);
    assert!(
        !copy.init_sql_needs_confirmation,
        "a duplicate is not imported"
    );
    assert!(!hub.read_with(cx, |h, _| h.init_sql_marker(plain_id)));

    // The core reports the confirmation: the hub clears its copy, and a form opened before it
    // does not bring the marker back.
    let stale = ConnectionForm::from_connection(&marked);
    hub.update(cx, |h, cx| {
        h.on_core_event(
            ninesql_core::CoreEvent::ImportedInitSqlConfirmed {
                connection: marked_id,
                statements: marked.init_sql.clone(),
                cleared: Some(true),
            },
            cx,
        );
    });
    assert!(!hub.read_with(cx, |h, _| h.init_sql_marker(marked_id)));
    let saved = save_form(cx, &hub, &stale, vec![]).unwrap_or_else(|e| panic!("{e}"));
    assert!(
        !saved.init_sql_needs_confirmation,
        "a stale form does not re-set the marker"
    );
    assert_eq!(saved.init_sql, marked.init_sql, "the init SQL is kept");
}

/// P1-E review, M1: the hub clears its marker only for the confirmed statements, and only when
/// the core recorded the confirmation (or has no workspace).
#[gpui::test]
fn a_confirmation_of_other_init_sql_or_a_failed_write_keeps_the_marker(cx: &mut TestAppContext) {
    let mut marked = imported_pg("Imported");
    marked.init_sql = vec!["SET search_path TO s2".into()];
    marked.init_sql_needs_confirmation = true;
    let id = marked.id;
    let mut config = HubConfig::empty();
    config.registry = shipped();
    config.connections =
        ninesql_core::Connections::from_vec(vec![marked.clone()]).unwrap_or_else(|e| panic!("{e}"));
    let hub = init(cx, config);
    let deliver = |cx: &mut TestAppContext, statements: Vec<String>, cleared: Option<bool>| {
        hub.update(cx, |h, cx| {
            h.on_core_event(
                ninesql_core::CoreEvent::ImportedInitSqlConfirmed {
                    connection: id,
                    statements,
                    cleared,
                },
                cx,
            );
        });
    };
    // The dialog showed S1; a re-import changed the copy to S2 meanwhile.
    for cleared in [Some(true), Some(false), None] {
        deliver(cx, vec!["SET search_path TO s1".into()], cleared);
        assert!(
            hub.read_with(cx, |h, _| h.init_sql_marker(id)),
            "{cleared:?}"
        );
    }
    // The right statements, but the core could not record the confirmation.
    deliver(cx, marked.init_sql.clone(), Some(false));
    assert!(
        hub.read_with(cx, |h, _| h.init_sql_marker(id)),
        "failed write"
    );
    // No workspace in the core: the statements decide.
    deliver(cx, marked.init_sql.clone(), None);
    assert!(!hub.read_with(cx, |h, _| h.init_sql_marker(id)));
}
