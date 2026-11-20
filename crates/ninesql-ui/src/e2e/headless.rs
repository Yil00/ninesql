//! The runner itself, headless (tests only): `TestAppContext` instead of a real window (no
//! frame loop, so checkpoints do not wait for a presented frame), an in-memory secret store
//! instead of the run's vault, and the core's test-build guards installed for one run
//! directory per test process (they are process-wide). Dev engines are skipped when
//! unreachable unless `NINESQL_REQUIRE_ENGINES` lists them.

use std::cell::RefCell;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use gpui::{TestAppContext, VisualTestContext};
use ninesql_core::secrets::{MemoryStore, SecretStores, StoreKind};
use ninesql_core::storage::{Paths, ProjectName, XdgDirs};
use ninesql_core::{KnownHostsFiles, ManagerConfig, SessionManager, Workspace as CoreWorkspace};

use super::runner::{self, Outcome, RunReport, RunnerConfig};
use super::scenario::{Scenario, repository_root};
use super::setup::{self, DevEngines};
use crate::connections::hub::{HubConfig, install, registry_with};
use crate::settings::{self, SettingsState};

/// `e2e/scenarios/`.
pub(super) fn scenarios_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../e2e/scenarios")
}

/// The run directory of this test process: private, under the temporary directory, the one
/// the core's guards are installed for.
fn run_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let tmp = tempfile::Builder::new()
            .prefix("ninesql-e2e-headless-")
            .tempdir()
            .unwrap_or_else(|e| panic!("{e}"))
            .keep();
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o700))
            .unwrap_or_else(|e| panic!("{e}"));
        let dir = std::fs::canonicalize(&tmp).unwrap_or_else(|e| panic!("{e}"));
        ninesql_core::e2e::install(&dir).unwrap_or_else(|e| panic!("{e}"));
        dir
    })
}

fn required(engine: &str) -> bool {
    std::env::var("NINESQL_REQUIRE_ENGINES")
        .unwrap_or_default()
        .split([',', ' '])
        .any(|e| e.trim() == engine || e.trim() == "all")
}

/// Runs the scenario `name` of `e2e/scenarios/` headless; `None` when skipped. Returns the
/// report and the scenario's directory (under the run directory).
pub(super) fn run_scenario(cx: &mut TestAppContext, name: &str) -> Option<(RunReport, PathBuf)> {
    if ninesql_core::e2e::refuse_secret_service().is_err() {
        return None; // `--all-features`: a test build with the keyring refuses to run.
    }
    let file = scenarios_dir().join(format!("{name}.toml"));
    let scenario = Scenario::load(&file).unwrap_or_else(|e| panic!("{e}"));
    let canonical = std::fs::canonicalize(&file).unwrap_or_else(|e| panic!("{e}"));
    let repo = repository_root(&canonical).unwrap_or_else(|| panic!("the repository"));
    let engines = scenario
        .engines
        .as_deref()
        .map(|p| DevEngines::load(p).unwrap_or_else(|e| panic!("{e}")));
    for id in scenario.dev_engines() {
        let up = engines
            .as_ref()
            .and_then(|e| e.get(id))
            .is_some_and(setup::reachable);
        if !up {
            assert!(!required(id), "{id} is required but unreachable");
            #[allow(clippy::print_stderr)] // Test output: why the test did nothing.
            {
                eprintln!("skipped: {id} unreachable (python3 dev/ready.py up core)");
            }
            return None;
        }
    }
    let root = run_dir().join(format!("{name}-{}", uuid::Uuid::new_v4().simple()));
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap_or_else(|e| panic!("{e}"));
    setup::prepare_home(&home, &scenario, &repo).unwrap_or_else(|e| panic!("{e}"));

    let r = root.clone();
    let paths = Paths::new(
        &XdgDirs::from_lookup(move |k| {
            let sub = match k {
                "XDG_CONFIG_HOME" => "config",
                "XDG_DATA_HOME" => "data",
                "XDG_STATE_HOME" => "state",
                "HOME" => "home",
                _ => return None,
            };
            Some(r.join(sub).into_os_string())
        })
        .unwrap_or_else(|e| panic!("{e}")),
    );
    let mut stores = SecretStores::new();
    stores.register(Arc::new(MemoryStore::new(StoreKind::Vault)));
    let registry = registry_with(vec![
        Arc::new(ninesql_driver_pgwire::PgWireDriver::new()),
        Arc::new(ninesql_driver_mysqlwire::MySqlWireDriver::new()),
        Arc::new(ninesql_driver_sqlite::SqliteDriver::new()),
    ]);
    let mut manager = ManagerConfig::with_known_hosts(KnownHostsFiles {
        own: paths.known_hosts_file(),
        user: None,
    });
    manager.workers = 2;
    let manager = SessionManager::new(manager, registry.clone(), stores.clone())
        .unwrap_or_else(|e| panic!("{e}"));
    let workspace = CoreWorkspace::open(paths.clone(), ProjectName::default_project())
        .unwrap_or_else(|e| panic!("{e}"));
    let mut config = HubConfig::empty();
    config.registry = registry;
    config.transfer = Some(ninesql_core::TransferService::new(manager.clone(), &paths));
    config.manager = Some(manager);
    config.workspace = Some(workspace);
    config.stores = stores;
    let (s, diagnostics) = settings::parse("");
    cx.update(|cx| {
        crate::app::init(
            SettingsState {
                path: None,
                settings: s,
                diagnostics,
                save_blocked: None,
            },
            cx,
        );
        install(config, cx);
    });
    let Some((window, workspace)) = cx.update(crate::app::open_window) else {
        panic!("the test platform opens windows");
    };
    let mut vcx = VisualTestContext::from_window(window, cx);
    vcx.executor().allow_parking();
    let lines: Rc<RefCell<Vec<String>>> = Rc::default();
    let sink = lines.clone();
    let config = RunnerConfig {
        scenario: scenario.clone(),
        engines,
        home: home.clone(),
        window,
        workspace,
        wait_for_frame: false,
        checkpoint_sink: Box::new(move |l| sink.borrow_mut().push(l.to_owned())),
    };
    let slot: Rc<RefCell<Option<RunReport>>> = Rc::default();
    let out = slot.clone();
    vcx.update(|_, cx| {
        let task = runner::spawn(config, cx);
        cx.spawn(async move |_| {
            *out.borrow_mut() = Some(task.await);
        })
        .detach();
    });
    let deadline = Instant::now() + scenario.timeout;
    loop {
        vcx.run_until_parked();
        if slot.borrow().is_some() {
            break;
        }
        assert!(Instant::now() < deadline, "the scenario did not end");
        vcx.executor().advance_clock(Duration::from_millis(50));
        std::thread::sleep(Duration::from_millis(5));
    }
    let report = slot.borrow_mut().take().unwrap_or_default();
    let expected: Vec<String> = report
        .checkpoints
        .iter()
        .enumerate()
        .map(|(i, n)| format!("ninesql-e2e checkpoint {} {n}", i + 1))
        .collect();
    assert_eq!(*lines.borrow(), expected, "one stdout line per checkpoint");
    Some((report, root))
}

/// Fails with every step that did not pass.
pub(super) fn assert_passed(report: &RunReport) {
    let failed: Vec<String> = report
        .steps
        .iter()
        .filter(|s| s.outcome != Outcome::Pass)
        .map(|s| format!("{} {} {:?}: {:?}", s.index, s.name, s.outcome, s.message))
        .collect();
    assert!(
        report.setup_error.is_none() && failed.is_empty(),
        "setup: {:?}\n{}",
        report.setup_error,
        failed.join("\n")
    );
}
