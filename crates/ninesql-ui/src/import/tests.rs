//! Tests of the DBeaver import UI (P2-B): the preview view model (what maps, disabled and not
//! imported rows, secrets as counts and fields, tightened types, update diffs with the TLS
//! toggle), the flow against a real `CoreServices` (preview, `VaultNotCreated` /
//! `VaultLocked` / `SourceChanged`, apply, hub reloaded from `ImportOutcome::connections`, the
//! report file and the advice), I1 (no fixture secret in any view or file) and I4 (the DBeaver
//! files are byte-identical), the "Test" button (credentials always asked), the first-run
//! offer, and "Enable connection" with the route confirmation.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use gpui::{Entity, TestAppContext, VisualTestContext};
use gpui_kit::component::WindowExt as _;
use ninesql_core::dbeaver::mapping::CredentialsStatus;
use ninesql_core::dbeaver::{InstallKind, SearchRoots};
use ninesql_core::secrets::{SecretString, StoreKind};
use ninesql_core::storage::{Paths, ProjectName, XdgDirs};
use ninesql_core::{
    Auth, Connection, ConnectionPlan, ConnectionType, ConnectionTypeId, Connections, CoreServices,
    DisabledKind, DisabledReason, Endpoint, EngineId, ImportPreview, ImportSource, ImportStatus,
    PlanAction, ProjectPreview, SecretChange, SecretChangeKind, SecretSlot, SecretSource,
    TlsChange, TlsMode, TypeAction, TypePlan, UpdateDiff,
};

use super::flow::{ImportFlow, RowTest, SOURCE_CHANGED_NOTICE, Stage};
use super::model::{
    DELETE_DBEAVER_PASSWORDS_ADVICE, INIT_SQL_NOTE, PreviewView, RowKind, Selection, TypeRowKind,
};
use crate::app;
use crate::connections::connect_flow::{FlowResult, PendingPrompt};
use crate::connections::hub::{ConnectionHub, DetectedDbeaver, HubConfig, install, registry_with};
use crate::connections::vault::VaultMode;
use crate::settings::{self, SettingsState};
use crate::workspace::Workspace as Shell;

const FIXTURE_SECRETS: &[&str] = &[
    "fixture-not-a-secret",
    "fixture-prod-not-a-secret",
    "fixture-ssh-not-a-secret",
    "fixture-passphrase-not-a-secret",
];
const MASTER: &str = "correct horse battery staple";

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
    vcx.executor().allow_parking();
    (ws, vcx)
}

fn wait_until(vcx: &mut VisualTestContext, mut done: impl FnMut(&mut VisualTestContext) -> bool) {
    vcx.executor().allow_parking();
    for _ in 0..1000 {
        vcx.run_until_parked();
        if done(vcx) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("condition not reached");
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

fn temp_paths(base: &Path) -> Paths {
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

/// A real `CoreServices` with the shipped drivers under `base` (no Secret Service: secrets go
/// to the vault, not created yet), installed as the hub.
fn core_hub(cx: &mut TestAppContext, base: &Path) -> (Entity<ConnectionHub>, CoreServices) {
    let mut config =
        ninesql_core::CoreConfig::new(temp_paths(base), ProjectName::default_project(), shipped());
    config.use_secret_service = false;
    config.watch_desktop_lock = false;
    config.manager.workers = 1;
    let core = CoreServices::start_blocking(config).unwrap_or_else(|e| panic!("{e}"));
    let hub = init(cx, HubConfig::load_from_core(core.clone()));
    (hub, core)
}

fn create_test_vault(core: &CoreServices) {
    wait_reply(core.create_vault_with(
        SecretString::from(MASTER.to_owned()),
        ninesql_core::secrets::KdfParams::insecure_for_tests(),
    ))
    .unwrap_or_else(|e| panic!("{e}"));
}

fn fixture(version: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../ninesql-dbeaver/tests/fixtures")
        .join(format!("dbeaver-{version}"))
        .join("workspace6")
}

fn copy_dir(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap_or_else(|e| panic!("{e}"));
    for entry in std::fs::read_dir(src).unwrap_or_else(|e| panic!("{e}")) {
        let entry = entry.unwrap_or_else(|e| panic!("{e}"));
        let to = dst.join(entry.file_name());
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            copy_dir(&entry.path(), &to);
        } else {
            std::fs::copy(entry.path(), &to).unwrap_or_else(|e| panic!("{e}"));
        }
    }
}

/// Every file under `dir` with its bytes and modification time.
fn tree(dir: &Path) -> BTreeMap<PathBuf, (Vec<u8>, SystemTime)> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(meta) = std::fs::metadata(&path) {
                let bytes = std::fs::read(&path).unwrap_or_default();
                let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                out.insert(path, (bytes, modified));
            }
        }
    }
    out
}

fn assert_no_secret_in_files(dir: &Path) {
    for (path, (bytes, _)) in tree(dir) {
        for needle in FIXTURE_SECRETS {
            assert!(
                !bytes.windows(needle.len()).any(|w| w == needle.as_bytes()),
                "a secret was found in {}",
                path.display()
            );
        }
    }
}

/// A DBeaver workspace with one `General` project holding `connections` (JSON members).
fn crafted(root: &Path, connections: &[String]) -> PathBuf {
    let ws = root.join("dbeaver").join("workspace6");
    let dot = ws.join("General").join(".dbeaver");
    std::fs::create_dir_all(&dot).unwrap_or_else(|e| panic!("{e}"));
    std::fs::create_dir_all(ws.join(".metadata")).unwrap_or_else(|e| panic!("{e}"));
    write_sources(&ws, connections);
    ws
}

fn write_sources(ws: &Path, connections: &[String]) {
    std::fs::write(
        ws.join("General/.dbeaver/data-sources.json"),
        format!(r#"{{ "connections": {{ {} }} }}"#, connections.join(", ")),
    )
    .unwrap_or_else(|e| panic!("{e}"));
}

fn pg(id: &str, name: &str) -> String {
    format!(
        r#""{id}": {{ "provider": "postgresql", "driver": "postgres-jdbc", "name": "{name}",
           "configuration": {{ "host": "db.example.invalid", "port": "5432", "database": "nine",
             "user": "nine_user", "type": "dev" }} }}"#
    )
}

fn oracle(id: &str, name: &str) -> String {
    format!(
        r#""{id}": {{ "provider": "oracle", "driver": "oracle_thin", "name": "{name}",
           "configuration": {{ "host": "ora.example.invalid", "port": "1521", "database": "XE",
             "user": "scott", "type": "dev" }} }}"#
    )
}

fn import_flow(ws: &Entity<Shell>, vcx: &mut VisualTestContext) -> Entity<ImportFlow> {
    ws.update_in(vcx, |ws, window, cx| ws.open_import_screen(window, cx))
}

fn preview(flow: &Entity<ImportFlow>, source: &Path, vcx: &mut VisualTestContext) {
    let source = source.to_path_buf();
    flow.update_in(vcx, |f, window, cx| {
        f.start(ImportSource::Folder(source), window, cx);
    });
    wait_until(vcx, |vcx| {
        flow.read_with(vcx, |f, _| !matches!(f.stage(), Stage::Previewing))
    });
}

fn stage(flow: &Entity<ImportFlow>, vcx: &VisualTestContext) -> Stage {
    flow.read_with(vcx, |f, _| f.stage().clone())
}

// ---------------------------------------------------------------------------------------------
// View model on hand-made previews.

fn pg_connection(name: &str, ty: ConnectionTypeId) -> Connection {
    Connection::new(
        name,
        EngineId::new("postgresql").unwrap_or_else(|e| panic!("{e}")),
        Endpoint::Tcp {
            host: "db.example.test".into(),
            port: Some(5432),
            database: Some("nine".into()),
        },
        Auth::Password {
            username: "app".into(),
            secret: SecretSource::Prompt,
        },
        ty,
    )
}

fn plan(
    id: &str,
    c: Option<Connection>,
    status: ImportStatus,
    action: PlanAction,
) -> ConnectionPlan {
    ConnectionPlan {
        project: "General".into(),
        dbeaver_id: id.into(),
        name: c.as_ref().map_or_else(|| id.to_owned(), |c| c.name.clone()),
        status,
        action,
        connection: c,
        secrets: Vec::new(),
        issues: Vec::new(),
    }
}

fn hand_preview() -> ImportPreview {
    let mut created = pg_connection("Created", ConnectionTypeId::dev());
    created.init_sql = vec!["SET search_path TO app".into()];
    created.init_sql_needs_confirmation = true;
    let mut created_plan = plan(
        "pg-created",
        Some(created),
        ImportStatus::Imported,
        PlanAction::Create,
    );
    created_plan.secrets = vec![ninesql_core::SecretToMove {
        slot: SecretSlot::Auth,
        kind: ninesql_core::secrets::SecretKind::Password,
    }];
    let updated = pg_connection("Updated", ConnectionTypeId::dev());
    let diff = UpdateDiff {
        tls: Some(TlsChange {
            saved: (TlsMode::VerifyFull, None),
            imported: (TlsMode::Require, None),
            kept_saved: true,
        }),
        secrets: vec![SecretChange {
            slot: SecretSlot::Auth,
            change: SecretChangeKind::Dropped,
        }],
        endpoint: Some(ninesql_core::Change {
            saved: Endpoint::Tcp {
                host: "old.example.test".into(),
                port: None,
                database: None,
            },
            imported: Endpoint::Tcp {
                host: "db.example.test".into(),
                port: Some(5432),
                database: Some("nine".into()),
            },
        }),
        ..UpdateDiff::default()
    };
    let updated_plan = plan(
        "pg-updated",
        Some(updated.clone()),
        ImportStatus::Imported,
        PlanAction::Update {
            existing: updated.id,
            diff: Box::new(diff),
        },
    );
    let mut route = pg_connection("Behind a proxy", ConnectionTypeId::dev());
    route.enabled = false;
    route.disabled_reason = Some(DisabledReason::new(
        DisabledKind::Route,
        "the DBeaver proxy handler is not supported",
    ));
    let route_plan = plan(
        "pg-route",
        Some(route),
        ImportStatus::ImportedDisabled,
        PlanAction::Create,
    );
    let failed_plan = plan(
        "broken",
        None,
        ImportStatus::Failed,
        PlanAction::NotImported {
            reason: "the data source has no driver id".into(),
        },
    );
    let mut stricter_test = ConnectionType::test();
    stricter_test.confirm_destructive = true;
    stricter_test.confirm_every_write = true;
    ImportPreview {
        source: ImportSource::Folder("/nowhere/workspace6".into()),
        workspaces: vec![ninesql_core::PreviewWorkspace {
            kind: InstallKind::Custom,
            path: "/nowhere/workspace6".into(),
            product_version: Some("25.3.5".into()),
        }],
        projects: vec![ProjectPreview {
            name: "General".into(),
            path: "/nowhere/workspace6/General".into(),
            dbeaver_workspace: "/nowhere/workspace6".into(),
            credentials: CredentialsStatus::Decrypted { passwords: 1 },
            folders: Vec::new(),
            connections: vec![created_plan, updated_plan, route_plan, failed_plan],
            issues: Vec::new(),
        }],
        connection_types: vec![TypePlan {
            dbeaver_id: "test".into(),
            id: ConnectionTypeId::test(),
            name: "Test".into(),
            action: TypeAction::Tighten(stricter_test),
        }],
        issues: Vec::new(),
        default_store: Some(StoreKind::Vault),
    }
}

#[test]
fn the_view_shows_what_maps_what_is_disabled_and_what_is_not_imported() {
    let saved_test = pg_connection("Existing test", ConnectionTypeId::test());
    let saved = Connections::from_vec(vec![saved_test]).unwrap_or_else(|e| panic!("{e}"));
    let preview = hand_preview();
    let view = PreviewView::build(&preview, &ninesql_core::ConnectionTypes::default(), &saved);
    assert_eq!(view.secret_count, 1);
    assert_eq!(
        view.sources,
        ["/nowhere/workspace6 (chosen folder, DBeaver 25.3.5)"]
    );

    let created = view.row("pg-created").unwrap_or_else(|| panic!("row"));
    assert_eq!(created.kind, RowKind::Create);
    assert_eq!(created.secrets, ["Password (database)"]);
    assert_eq!(created.init_sql, 1);
    assert!(created.testable && created.skippable);

    let route = view.row("pg-route").unwrap_or_else(|| panic!("row"));
    assert_eq!(route.kind, RowKind::Disabled);
    assert!(route.route_reason);
    assert_eq!(
        route.reason.as_deref(),
        Some("route not reproduced: the DBeaver proxy handler is not supported")
    );
    assert!(!route.testable && route.skippable);

    let broken = view.row("broken").unwrap_or_else(|| panic!("row"));
    assert_eq!(broken.kind, RowKind::NotImported);
    assert_eq!(
        broken.reason.as_deref(),
        Some("the data source has no driver id")
    );
    assert!(!broken.skippable && !broken.testable);

    // The update: TLS kept at the saved setting, the saved password dropped.
    let updated = view.row("pg-updated").unwrap_or_else(|| panic!("row"));
    assert_eq!(updated.kind, RowKind::Update);
    assert!(updated.tls_kept());
    let diff = updated.diff.clone().unwrap_or_else(|| panic!("diff"));
    assert_eq!(diff.secrets_dropped, ["database"]);
    assert!(
        diff.changes
            .iter()
            .any(|c| c == "Endpoint: old.example.test → db.example.test:5432/nine"),
        "{:?}",
        diff.changes
    );
    let tls = diff.tls.unwrap_or_else(|| panic!("tls"));
    assert_eq!(tls.saved, "Verify CA and host name");
    assert_eq!(tls.imported, "Encrypt only");

    // The built-in Test type is tightened and a saved connection uses it: clearly flagged.
    assert_eq!(view.types.len(), 1);
    let t = &view.types[0];
    assert_eq!(t.kind, TypeRowKind::Tighten);
    assert!(t.builtin && t.changes_existing());
    assert_eq!(t.existing_connections, 1);
    let text = t.text();
    assert!(
        text.contains("built-in type \"Test\" becomes stricter"),
        "{text}"
    );
    assert!(
        text.contains("asks before destructive statements"),
        "{text}"
    );
    assert!(
        text.contains("This changes 1 saved connection(s)"),
        "{text}"
    );
    assert_eq!(view.tightened_in_use().count(), 1);
}

#[test]
fn choices_follow_the_selection_and_only_offered_rows() {
    let preview = hand_preview();
    let view = PreviewView::build(
        &preview,
        &ninesql_core::ConnectionTypes::default(),
        &Connections::default(),
    );
    let mut sel = Selection::default();
    assert_eq!(
        sel.selected_count(&view),
        3,
        "the not-imported row is not counted"
    );
    let defaults = sel.choices(&view);
    assert_eq!(defaults.store, Some(StoreKind::Vault));
    assert!(
        defaults.accept_tls_changes.is_empty(),
        "never accepted by default (I5)"
    );
    assert!(defaults.update_existing);

    sel.skip.insert("pg-route".into());
    sel.skip.insert("broken".into()); // not skippable: never passed
    sel.accept_tls.insert("pg-updated".into());
    sel.accept_tls.insert("pg-created".into()); // no kept TLS: never passed
    sel.import_secrets = false;
    let choices = sel.choices(&view);
    assert_eq!(choices.store, None);
    assert_eq!(choices.skip.iter().collect::<Vec<_>>(), ["pg-route"]);
    assert_eq!(
        choices.accept_tls_changes.iter().collect::<Vec<_>>(),
        ["pg-updated"]
    );
    assert_eq!(sel.selected_count(&view), 2);
    sel.update_existing = false;
    assert_eq!(sel.selected_count(&view), 1);
}

#[gpui::test]
fn the_tls_toggle_only_applies_to_kept_settings_and_resets_on_a_new_preview(
    cx: &mut TestAppContext,
) {
    let mut config = HubConfig::empty();
    config.registry = shipped();
    init(cx, config);
    let (ws, mut vcx) = open(cx);
    let flow = import_flow(&ws, &mut vcx);
    flow.update_in(&mut vcx, |f, window, cx| {
        f.on_preview(Some(Ok(hand_preview())), window, cx);
    });
    assert_eq!(stage(&flow, &vcx), Stage::Preview);
    flow.update(&mut vcx, |f, cx| {
        f.toggle_accept_tls("pg-updated", cx);
        f.toggle_accept_tls("pg-created", cx);
        f.toggle_skip("broken", cx);
        f.toggle_skip("pg-route", cx);
    });
    let sel = flow.read_with(&vcx, |f, _| f.selection().clone());
    assert_eq!(sel.accept_tls.iter().collect::<Vec<_>>(), ["pg-updated"]);
    assert_eq!(sel.skip.iter().collect::<Vec<_>>(), ["pg-route"]);
    // The source changed: skips are kept, an accepted TLS change is not (I5).
    flow.update_in(&mut vcx, |f, window, cx| {
        f.on_preview(Some(Ok(hand_preview())), window, cx);
    });
    let sel = flow.read_with(&vcx, |f, _| f.selection().clone());
    assert!(sel.accept_tls.is_empty());
    assert_eq!(sel.skip.iter().collect::<Vec<_>>(), ["pg-route"]);
}

// ---------------------------------------------------------------------------------------------
// The flow against a real core.

#[gpui::test]
fn preview_vault_creation_apply_reload_report_and_advice(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let base = std::fs::canonicalize(tmp.path()).unwrap_or_else(|e| panic!("{e}"));
    let source = base.join("dbeaver/workspace6");
    copy_dir(&fixture("26.2.4"), &source);
    let before = tree(&source);
    let (hub, core) = core_hub(cx, &base);
    let (ws, mut vcx) = open(cx);
    let flow = import_flow(&ws, &mut vcx);
    preview(&flow, &source, &mut vcx);
    assert_eq!(stage(&flow, &vcx), Stage::Preview);

    let view = flow.read_with(&vcx, |f, _| f.view().cloned());
    let Some(view) = view else { panic!("view") };
    assert!(view.secret_count >= 6, "{}", view.secret_count);
    assert_eq!(view.default_store, Some(StoreKind::Vault));
    // I1: no secret value anywhere in what the screen shows.
    let text = view.all_text();
    for secret in FIXTURE_SECRETS {
        assert!(!text.contains(secret));
    }
    // Disabled for want of a driver, with the reason; init SQL marked.
    let oracle = view
        .rows()
        .find(|r| r.name == "oracle-legacy")
        .unwrap_or_else(|| panic!("oracle row"));
    assert_eq!(oracle.kind, RowKind::Disabled);
    assert!(
        oracle
            .reason
            .as_deref()
            .is_some_and(|r| r.starts_with("no driver"))
    );
    let pg_dev = view
        .rows()
        .find(|r| r.name == "pg-dev")
        .unwrap_or_else(|| panic!("pg-dev row"));
    assert!(pg_dev.init_sql > 0 && INIT_SQL_NOTE.contains("first connect"));
    assert!(
        view.types
            .iter()
            .any(|t| t.kind == TypeRowKind::Tighten && t.builtin)
    );

    // Leave one out.
    let skipped = oracle.dbeaver_id.clone();
    flow.update(&mut vcx, |f, cx| f.toggle_skip(&skipped, cx));

    // The vault is not created: the create prompt opens, then the same preview is applied.
    flow.update_in(&mut vcx, |f, window, cx| f.apply(window, cx));
    wait_until(&mut vcx, |vcx| {
        !matches!(stage(&flow, vcx), Stage::Applying)
    });
    assert_eq!(
        stage(&flow, &vcx),
        Stage::WaitingForVault(VaultMode::Create)
    );
    let prompt = flow.read_with(&vcx, |f, _| f.vault_prompt());
    let Some(prompt) = prompt else {
        panic!("vault prompt")
    };
    assert_eq!(prompt.read_with(&vcx, |p, _| p.mode()), VaultMode::Create);
    assert!(hub.read_with(&vcx, |h, _| h.connections().is_empty()));
    create_test_vault(&core);
    vcx.update(|window, cx| prompt.update(cx, |p, cx| p.apply_result(Some(Ok(())), window, cx)));
    wait_until(&mut vcx, |vcx| {
        matches!(stage(&flow, vcx), Stage::Done(_) | Stage::Preview)
    });
    let Stage::Done(outcome) = stage(&flow, &vcx) else {
        panic!(
            "not imported: {:?}",
            flow.read_with(&vcx, |f, _| f.notice().map(str::to_owned))
        );
    };
    assert_eq!(outcome.advice, Some(DELETE_DBEAVER_PASSWORDS_ADVICE));
    assert!(
        outcome.lines.iter().any(|l| l.contains("1 left out")),
        "{:?}",
        outcome.lines
    );
    let report = PathBuf::from(&outcome.report_file);
    assert!(report.is_file(), "{}", report.display());
    assert!(report.starts_with(&base), "{}", report.display());

    // The hub holds the list as written (L-d): every imported connection, not the skipped one.
    let disk = core
        .workspace()
        .load_connections()
        .unwrap_or_else(|e| panic!("{e}"))
        .connections;
    let names = |c: &Connections| {
        let mut n: Vec<String> = c.iter().map(|c| c.name.clone()).collect();
        n.sort();
        n
    };
    assert_eq!(
        hub.read_with(&vcx, |h, _| names(h.connections())),
        names(&disk)
    );
    assert!(!names(&disk).contains(&"oracle-legacy".to_owned()));
    assert!(!disk.is_empty());
    // The tightened built-in type was read again.
    let test_type = hub.read_with(&vcx, |h, _| h.types().resolve(&ConnectionTypeId::test()));
    assert!(test_type.confirm_destructive);

    // A save after the import keeps every imported connection (no stale list).
    let new = pg_connection("Added after import", ConnectionTypeId::dev());
    let task = hub.update(&mut vcx, |h, cx| {
        h.save(
            crate::connections::form::BuiltConnection {
                connection: new,
                to_store: Vec::new(),
                to_delete: Vec::new(),
            },
            cx,
        )
    });
    let done = std::rc::Rc::new(std::cell::RefCell::new(None));
    let slot = done.clone();
    vcx.foreground_executor()
        .spawn(async move { *slot.borrow_mut() = Some(task.await) })
        .detach();
    wait_until(&mut vcx, |_| done.borrow().is_some());
    let saved = done.borrow_mut().take();
    assert!(matches!(saved, Some(Ok(_))), "{saved:?}");
    let after = core
        .workspace()
        .load_connections()
        .unwrap_or_else(|e| panic!("{e}"))
        .connections;
    assert_eq!(after.len(), disk.len() + 1);

    // I4: the DBeaver files are byte-identical; I1: no fixture secret in any NineSQL file.
    assert_eq!(tree(&source), before, "DBeaver files changed");
    assert_no_secret_in_files(&base.join("data"));
    assert_no_secret_in_files(&base.join("config"));
    assert_no_secret_in_files(&base.join("state"));
}

#[gpui::test]
fn a_locked_vault_is_unlocked_then_the_import_is_applied_again(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let base = std::fs::canonicalize(tmp.path()).unwrap_or_else(|e| panic!("{e}"));
    let source = base.join("dbeaver/workspace6");
    copy_dir(&fixture("26.2.4"), &source);
    let (hub, core) = core_hub(cx, &base);
    create_test_vault(&core);
    wait_reply(core.lock_now());
    let (ws, mut vcx) = open(cx);
    let flow = import_flow(&ws, &mut vcx);
    preview(&flow, &source, &mut vcx);
    flow.update_in(&mut vcx, |f, window, cx| f.apply(window, cx));
    wait_until(&mut vcx, |vcx| {
        !matches!(stage(&flow, vcx), Stage::Applying)
    });
    assert_eq!(
        stage(&flow, &vcx),
        Stage::WaitingForVault(VaultMode::Unlock)
    );
    assert!(
        hub.read_with(&vcx, |h, _| h.connections().is_empty()),
        "nothing written"
    );
    let Some(prompt) = flow.read_with(&vcx, |f, _| f.vault_prompt()) else {
        panic!("prompt")
    };
    wait_reply(core.unlock_vault(SecretString::from(MASTER.to_owned())))
        .unwrap_or_else(|e| panic!("{e}"));
    vcx.update(|window, cx| prompt.update(cx, |p, cx| p.apply_result(Some(Ok(())), window, cx)));
    wait_until(&mut vcx, |vcx| {
        matches!(stage(&flow, vcx), Stage::Done(_) | Stage::Preview)
    });
    assert!(
        matches!(stage(&flow, &vcx), Stage::Done(_)),
        "{:?}",
        stage(&flow, &vcx)
    );
    assert!(!hub.read_with(&vcx, |h, _| h.connections().is_empty()));
}

#[gpui::test]
fn a_cancelled_vault_prompt_writes_nothing_and_says_why(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let base = std::fs::canonicalize(tmp.path()).unwrap_or_else(|e| panic!("{e}"));
    let source = base.join("dbeaver/workspace6");
    copy_dir(&fixture("26.2.4"), &source);
    let (hub, _core) = core_hub(cx, &base);
    let (ws, mut vcx) = open(cx);
    let flow = import_flow(&ws, &mut vcx);
    preview(&flow, &source, &mut vcx);
    flow.update_in(&mut vcx, |f, window, cx| f.apply(window, cx));
    wait_until(&mut vcx, |vcx| {
        !matches!(stage(&flow, vcx), Stage::Applying)
    });
    let Some(prompt) = flow.read_with(&vcx, |f, _| f.vault_prompt()) else {
        panic!("prompt")
    };
    vcx.update(|window, cx| prompt.update(cx, |p, cx| p.cancel(window, cx)));
    vcx.run_until_parked();
    assert_eq!(stage(&flow, &vcx), Stage::Preview);
    let notice = flow.read_with(&vcx, |f, _| f.notice().map(str::to_owned));
    assert!(notice.is_some_and(|n| n.contains("Import saved passwords")));
    assert!(hub.read_with(&vcx, |h, _| h.connections().is_empty()));

    // Without the passwords, the import needs no store.
    flow.update(&mut vcx, |f, cx| f.set_import_secrets(false, cx));
    flow.update_in(&mut vcx, |f, window, cx| f.apply(window, cx));
    wait_until(&mut vcx, |vcx| {
        matches!(stage(&flow, vcx), Stage::Done(_) | Stage::Preview)
    });
    let Stage::Done(outcome) = stage(&flow, &vcx) else {
        panic!(
            "not imported: {:?}",
            flow.read_with(&vcx, |f, _| f.notice().map(str::to_owned))
        )
    };
    assert!(
        outcome
            .lines
            .iter()
            .any(|l| l.starts_with("0 saved password(s) moved")),
        "{:?}",
        outcome.lines
    );
    // Saved passwords were found in DBeaver: the advice is still shown.
    assert_eq!(outcome.advice, Some(DELETE_DBEAVER_PASSWORDS_ADVICE));
}

#[gpui::test]
fn a_source_changed_since_the_preview_is_previewed_again_with_a_notice(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let base = std::fs::canonicalize(tmp.path()).unwrap_or_else(|e| panic!("{e}"));
    let source = crafted(&base, &[pg("pg-a", "A")]);
    let (hub, _core) = core_hub(cx, &base);
    let (ws, mut vcx) = open(cx);
    let flow = import_flow(&ws, &mut vcx);
    preview(&flow, &source, &mut vcx);
    assert_eq!(
        flow.read_with(&vcx, |f, _| f.view().map(|v| v.rows().count())),
        Some(1)
    );
    write_sources(&source, &[pg("pg-a", "A"), pg("pg-b", "B")]);
    flow.update_in(&mut vcx, |f, window, cx| f.apply(window, cx));
    wait_until(&mut vcx, |vcx| {
        flow.read_with(vcx, |f, _| {
            matches!(f.stage(), Stage::Preview) && f.view().is_some_and(|v| v.rows().count() == 2)
        })
    });
    assert_eq!(
        flow.read_with(&vcx, |f, _| f.notice().map(str::to_owned))
            .as_deref(),
        Some(SOURCE_CHANGED_NOTICE)
    );
    assert!(
        hub.read_with(&vcx, |h, _| h.connections().is_empty()),
        "nothing written"
    );
}

#[gpui::test]
fn test_asks_for_credentials_under_the_previewed_connection(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let base = std::fs::canonicalize(tmp.path()).unwrap_or_else(|e| panic!("{e}"));
    let source = crafted(&base, &[pg("pg-a", "A"), oracle("ora", "Legacy")]);
    let (_hub, _core) = core_hub(cx, &base);
    let (ws, mut vcx) = open(cx);
    let flow = import_flow(&ws, &mut vcx);
    preview(&flow, &source, &mut vcx);
    let testable = flow.read_with(&vcx, |f, _| {
        f.view().map(|v| {
            v.rows()
                .map(|r| (r.dbeaver_id.clone(), r.testable))
                .collect::<Vec<_>>()
        })
    });
    assert_eq!(
        testable,
        Some(vec![("ora".to_owned(), false), ("pg-a".to_owned(), true)])
    );
    flow.update_in(&mut vcx, |f, window, cx| f.test("pg-a", window, cx));
    let connect = ws.read_with(&vcx, |ws, _| ws.flow().clone());
    wait_until(&mut vcx, |vcx| {
        connect.read_with(vcx, |c, _| matches!(c.prompt(), PendingPrompt::Secrets(_)))
    });
    assert_eq!(
        flow.read_with(&vcx, |f, _| f.test_state("pg-a").cloned()),
        Some(RowTest::Running)
    );
    // Cancelled: the row says so, and no notification comes from the window.
    vcx.update(|window, cx| window.clear_notifications(cx));
    vcx.update(|window, cx| connect.update(cx, |c, cx| c.cancel(window, cx)));
    vcx.run_until_parked();
    assert_eq!(
        flow.read_with(&vcx, |f, _| f.test_state("pg-a").cloned()),
        Some(RowTest::Done(FlowResult::Cancelled))
    );
    // A disabled row cannot be tested.
    flow.update_in(&mut vcx, |f, window, cx| f.test("ora", window, cx));
    assert!(flow.read_with(&vcx, |f, _| f.test_state("ora").is_none()));
}

// ---------------------------------------------------------------------------------------------
// Entry points.

#[gpui::test]
fn the_import_key_opens_the_import_screen(cx: &mut TestAppContext) {
    let mut config = HubConfig::empty();
    config.registry = shipped();
    init(cx, config);
    let (ws, mut vcx) = open(cx);
    vcx.simulate_keystrokes("ctrl-k ctrl-i");
    assert!(vcx.update(|window, cx| window.has_active_dialog(cx)));
    let flow = ws.read_with(&vcx, |ws, _| ws.import_flow().cloned());
    let Some(flow) = flow else { panic!("flow") };
    // No core in this test: said so, with the folder chooser still offered.
    assert!(matches!(
        stage(&flow, &vcx),
        Stage::Failed(_) | Stage::ChooseFolder
    ));
    for name in [
        "ImportFromDbeaver",
        "ImportFromDbeaverFolder",
        "EnableConnection",
    ] {
        let spec = crate::actions::find(name).unwrap_or_else(|| panic!("{name}"));
        assert!(spec.in_palette && !spec.default_keys.is_empty());
    }
}

#[gpui::test]
fn the_first_run_offer_shows_and_is_remembered_when_dismissed(cx: &mut TestAppContext) {
    let mut config = HubConfig::empty();
    config.registry = shipped();
    let hub = init(cx, config);
    let (ws, mut vcx) = open(cx);
    assert!(
        ws.read_with(&vcx, |ws, cx| ws.first_run_offer(cx))
            .is_none(),
        "nothing found yet"
    );
    let found = vec![DetectedDbeaver {
        kind: InstallKind::Native,
        path: "/home/you/.local/share/DBeaverData/workspace6".into(),
        projects: 2,
    }];
    hub.update(&mut vcx, |h, cx| h.set_detected_dbeaver(found.clone(), cx));
    assert_eq!(
        ws.read_with(&vcx, |ws, cx| ws.first_run_offer(cx)),
        Some(found.clone())
    );

    // "Not now" hides it in this window only.
    ws.update(&mut vcx, |ws, cx| ws.dismiss_first_run(false, cx));
    assert!(
        ws.read_with(&vcx, |ws, cx| ws.first_run_offer(cx))
            .is_none()
    );
    let (ws2, mut vcx2) = open(cx);
    assert!(
        ws2.read_with(&vcx2, |ws, cx| ws.first_run_offer(cx))
            .is_some()
    );

    // "Don't show again" is saved in the settings.
    ws2.update(&mut vcx2, |ws, cx| ws.dismiss_first_run(true, cx));
    let on = vcx2.update(|_, cx| {
        cx.global::<SettingsState>()
            .settings
            .import
            .first_run_prompt
    });
    assert!(!on);
    let (ws3, vcx3) = open(cx);
    assert!(
        ws3.read_with(&vcx3, |ws, cx| ws.first_run_offer(cx))
            .is_none()
    );
}

#[gpui::test]
fn the_first_run_offer_needs_an_empty_project(cx: &mut TestAppContext) {
    let mut config = HubConfig::empty();
    config.registry = shipped();
    config.connections =
        Connections::from_vec(vec![pg_connection("Mine", ConnectionTypeId::dev())])
            .unwrap_or_else(|e| panic!("{e}"));
    let hub = init(cx, config);
    let (ws, mut vcx) = open(cx);
    hub.update(&mut vcx, |h, cx| {
        h.set_detected_dbeaver(
            vec![DetectedDbeaver {
                kind: InstallKind::Flatpak,
                path: "/x".into(),
                projects: 1,
            }],
            cx,
        );
    });
    assert!(
        ws.read_with(&vcx, |ws, cx| ws.first_run_offer(cx))
            .is_none()
    );
}

#[gpui::test]
fn detection_lists_workspaces_without_changing_them(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let home = std::fs::canonicalize(tmp.path()).unwrap_or_else(|e| panic!("{e}"));
    let native = home.join(".local/share/DBeaverData/workspace6");
    copy_dir(&fixture("26.2.4"), &native);
    let before = tree(&native);
    let mut config = HubConfig::empty();
    config.registry = shipped();
    let hub = init(cx, config);
    let (_ws, mut vcx) = open(cx);
    hub.update(&mut vcx, |h, cx| {
        h.detect_dbeaver(SearchRoots { home: home.clone() }, cx);
    });
    wait_until(&mut vcx, |vcx| {
        hub.read_with(vcx, |h, _| h.detected_dbeaver().is_some())
    });
    let found = hub.read_with(&vcx, |h, _| h.detected_dbeaver().map(<[_]>::to_vec));
    let Some(found) = found else { panic!("found") };
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].kind, InstallKind::Native);
    assert_eq!(found[0].path, native);
    assert!(found[0].projects >= 1);
    assert_eq!(tree(&native), before);
}

// ---------------------------------------------------------------------------------------------
// Navigator and "Enable connection".

fn disabled(name: &str, engine: &str, kind: DisabledKind, message: &str) -> Connection {
    let mut c = pg_connection(name, ConnectionTypeId::dev());
    c.engine = EngineId::new(engine).unwrap_or_else(|e| panic!("{e}"));
    c.enabled = false;
    c.disabled_reason = Some(DisabledReason::new(kind, message));
    c
}

#[gpui::test]
fn a_route_reason_needs_the_explicit_confirmation(cx: &mut TestAppContext) {
    let route = disabled(
        "Behind a proxy",
        "postgresql",
        DisabledKind::Route,
        "proxy handler not supported",
    );
    let id = route.id;
    let mut config = HubConfig::empty();
    config.registry = shipped();
    config.connections = Connections::from_vec(vec![route]).unwrap_or_else(|e| panic!("{e}"));
    let hub = init(cx, config);
    let (ws, mut vcx) = open(cx);
    let label = hub.read_with(&vcx, |h, _| {
        h.connection(id)
            .and_then(crate::navigator::row_disabled_label)
    });
    let Some((short, full)) = label else {
        panic!("label")
    };
    assert_eq!(short, "disabled: route not reproduced");
    assert!(full.contains("proxy handler not supported"));

    // Connect says why and does not start.
    ws.update(&mut vcx, |ws, cx| {
        ws.navigator()
            .update(cx, |n, cx| n.select_connection(id, cx));
    });
    vcx.update(|window, cx| window.clear_notifications(cx));
    vcx.simulate_keystrokes("ctrl-k ctrl-o");
    assert!(!ws.read_with(&vcx, |ws, cx| ws.flow().read(cx).is_busy()));
    assert_eq!(vcx.update(|window, cx| window.notifications(cx).len()), 1);

    // The action opens the prompt.
    vcx.simulate_keystrokes("ctrl-k ctrl-shift-e");
    assert!(vcx.update(|window, cx| window.has_active_dialog(cx)));
    vcx.update(|window, cx| window.close_dialog(cx));

    let prompt = vcx.update(|window, cx| {
        crate::import::enable::open_enable_prompt(hub.clone(), id, window, cx)
    });
    assert!(prompt.read_with(&vcx, |p, _| p.needs_route_confirmation()));
    assert!(!prompt.read_with(&vcx, |p, _| p.can_submit()));
    vcx.update(|window, cx| prompt.update(cx, |p, cx| p.submit(window, cx)));
    vcx.run_until_parked();
    assert!(prompt.read_with(&vcx, |p, _| p.error().is_some()));
    assert!(hub.read_with(&vcx, |h, _| h.connection(id).is_some_and(|c| !c.enabled)));

    prompt.update(&mut vcx, |p, cx| p.toggle_route_fixed(cx));
    assert!(prompt.read_with(&vcx, |p, _| p.can_submit()));
    vcx.update(|window, cx| prompt.update(cx, |p, cx| p.submit(window, cx)));
    wait_until(&mut vcx, |vcx| {
        hub.read_with(vcx, |h, _| h.connection(id).is_some_and(|c| c.enabled))
    });
    let c = hub.read_with(&vcx, |h, _| h.connection(id).cloned());
    assert!(c.is_some_and(|c| c.disabled_reason.is_none()));
}

#[gpui::test]
fn enabling_a_connection_without_a_driver_is_refused_with_the_reason(cx: &mut TestAppContext) {
    let c = disabled(
        "Legacy",
        "oracle",
        DisabledKind::NoDriver,
        "no NineSQL driver for Oracle",
    );
    let id = c.id;
    let mut config = HubConfig::empty();
    config.registry = shipped();
    config.connections = Connections::from_vec(vec![c]).unwrap_or_else(|e| panic!("{e}"));
    let hub = init(cx, config);
    let (_ws, mut vcx) = open(cx);
    let prompt = vcx.update(|window, cx| {
        crate::import::enable::open_enable_prompt(hub.clone(), id, window, cx)
    });
    assert!(!prompt.read_with(&vcx, |p, _| p.needs_route_confirmation()));
    vcx.update(|window, cx| prompt.update(cx, |p, cx| p.submit(window, cx)));
    wait_until(&mut vcx, |vcx| {
        prompt.read_with(vcx, |p, _| p.error().is_some())
    });
    let error = prompt.read_with(&vcx, |p, _| p.error().map(str::to_owned));
    assert!(error.is_some_and(|e| e.starts_with("Not enabled:")));
    assert!(hub.read_with(&vcx, |h, _| h.connection(id).is_some_and(|c| !c.enabled)));
    assert!(
        vcx.update(|window, cx| window.has_active_dialog(cx)),
        "stays open"
    );
}
