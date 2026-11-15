//! DBeaver import, core side (ROADMAP P2-C): conversion and validation, secrets moved to the
//! store (I1), connection types never weakened, idempotent re-import (F-CON-14), the imported
//! init SQL marker (ADR-0022 §1), disabled reasons, files inside a DBeaver workspace (I4), the
//! P0-E fixture snapshots, byte-identical DBeaver files after preview and apply (I4), and the
//! 100-connection timing (phase 2 exit criterion).
//!
//! Snapshots live in `tests/snapshots/`; regenerate them with `NINESQL_UPDATE_SNAPSHOTS=1` and
//! review the diff (they hold names, statuses, kinds and keys, never a secret value).
#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).

mod support;

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use ninesql_core::{
    ConnectionTypeId, Connections, CoreConfig, CoreServices, DbeaverRoots, DisabledKind,
    DriverRegistry, ImportChoices, ImportError, ImportPreview, ImportSource, ImportStatus,
    Importer, KnownHostsFiles, OpenOutcome, OpenRequest, PlanAction, SecretChange,
    SecretChangeKind, SecretSlot, TlsMode, TypeAction, Workspace,
};
use ninesql_secrets::{KdfParams, MemoryStore, SecretStores, SecretString, StoreKind};
use ninesql_storage::{Paths, ProjectName, XdgDirs};
use support::ui;

/// Fake secrets of the P0-E fixtures (crates/ninesql-dbeaver/tests/fixtures/README.md).
const FIXTURE_SECRETS: &[&str] = &[
    "fixture-not-a-secret",
    "fixture-prod-not-a-secret",
    "fixture-ssh-not-a-secret",
    "fixture-passphrase-not-a-secret",
];
const FIXTURE_VERSIONS: &[&str] = &["22.3.5", "23.3.5", "24.3.5", "25.3.5", "26.2.4"];
const CRAFTED_PASSWORD: &str = "Crafted-Pa55-7d1e";

fn paths(root: &Path) -> Paths {
    Paths::new(&XdgDirs {
        config_home: root.join("config"),
        data_home: root.join("data"),
        state_home: root.join("state"),
    })
}

fn registry() -> DriverRegistry {
    let mut r = DriverRegistry::new();
    r.register(Arc::new(ninesql_driver_pgwire::PgWireDriver::new()));
    r.register(Arc::new(ninesql_driver_mysqlwire::MySqlWireDriver::new()));
    r.register(Arc::new(ninesql_driver_sqlite::SqliteDriver::new()));
    r
}

struct Env {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    workspace: Workspace,
    store: Arc<MemoryStore>,
    roots: DbeaverRoots,
    importer: Importer,
}

fn env() -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(tmp.path()).unwrap();
    let workspace =
        Workspace::open(paths(&root.join("ninesql")), ProjectName::default_project()).unwrap();
    let store = Arc::new(MemoryStore::new(StoreKind::Vault));
    let mut stores = SecretStores::new();
    stores.register(Arc::clone(&store) as _);
    let roots = DbeaverRoots::none().with_home(root.join("home"));
    let importer = Importer::new(
        workspace.clone(),
        registry(),
        stores,
        roots.clone(),
        Some(StoreKind::Vault),
    );
    Env {
        _tmp: tmp,
        root,
        workspace,
        store,
        roots,
        importer,
    }
}

fn fixture(version: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../ninesql-dbeaver/tests/fixtures")
        .join(format!("dbeaver-{version}"))
        .join("workspace6")
}

fn copy_dir(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let to = dst.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &to);
        } else {
            std::fs::copy(entry.path(), &to).unwrap();
        }
    }
}

/// Every file under `dir` with its bytes and modification time.
fn tree(dir: &Path) -> BTreeMap<PathBuf, (Vec<u8>, SystemTime)> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                let meta = std::fs::metadata(&path).unwrap();
                out.insert(
                    path.clone(),
                    (std::fs::read(&path).unwrap(), meta.modified().unwrap()),
                );
            }
        }
    }
    out
}

fn assert_not_in_files(dir: &Path, needles: &[&str]) {
    for (path, (bytes, _)) in tree(dir) {
        for needle in needles {
            assert!(
                !bytes.windows(needle.len()).any(|w| w == needle.as_bytes()),
                "a secret was found in {}",
                path.display()
            );
        }
    }
}

/// A DBeaver workspace with one `General` project holding `data_sources`.
fn crafted(root: &Path, data_sources: &str) -> PathBuf {
    let ws = root.join("dbeaver").join("workspace6");
    let dot = ws.join("General").join(".dbeaver");
    std::fs::create_dir_all(&dot).unwrap();
    std::fs::create_dir_all(ws.join(".metadata")).unwrap();
    std::fs::write(dot.join("data-sources.json"), data_sources).unwrap();
    ws
}

fn pg(id: &str, name: &str, extra_config: &str) -> String {
    format!(
        r#""{id}": {{ "provider": "postgresql", "driver": "postgres-jdbc", "name": "{name}",
           "configuration": {{ "host": "db.example.invalid", "port": "5432", "database": "nine",
             "user": "nine_user", "type": "dev" {extra_config} }} }}"#
    )
}

fn plan<'a>(preview: &'a ImportPreview, name: &str) -> &'a ninesql_core::ConnectionPlan {
    preview.connections().find(|c| c.name == name).unwrap()
}

fn saved(workspace: &Workspace) -> Connections {
    workspace.load_connections().unwrap().connections
}

// ---------------------------------------------------------------------------------------------
// Log capture (same subscriber as tests/no_secret_on_disk.rs)

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<String>>);

struct Visitor<'a>(&'a mut String);

impl tracing::field::Visit for Visitor<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        let _ = write!(self.0, " {}={:?}", field.name(), value);
    }
}

impl tracing::Subscriber for Capture {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        span.record(&mut Visitor(&mut self.0.lock().unwrap()));
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, values: &tracing::span::Record<'_>) {
        values.record(&mut Visitor(&mut self.0.lock().unwrap()));
    }
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        let mut buf = self.0.lock().unwrap();
        buf.push('\n');
        event.record(&mut Visitor(&mut buf));
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

// ---------------------------------------------------------------------------------------------
// Snapshots

fn summary(preview: &ImportPreview) -> String {
    let mut out = String::new();
    for t in &preview.connection_types {
        let action = match &t.action {
            TypeAction::Create(t) => format!("create {:?}", t.policy()),
            TypeAction::Tighten(t) => format!("tighten {:?}", t.policy()),
            TypeAction::Keep => "keep".to_owned(),
        };
        let _ = writeln!(out, "type {} -> {}: {action}", t.dbeaver_id, t.id);
    }
    for p in &preview.projects {
        let _ = writeln!(out, "project {} credentials={:?}", p.name, p.credentials);
        for f in &p.folders {
            let _ = writeln!(out, "folder {} used={}", f.path.join("/"), f.used);
        }
        let mut connections: Vec<_> = p.connections.iter().collect();
        connections.sort_by(|a, b| a.name.cmp(&b.name));
        for c in connections {
            let action = match &c.action {
                PlanAction::Create => "create".to_owned(),
                PlanAction::Update { .. } => "update".to_owned(),
                PlanAction::NotImported { reason } => format!("not-imported ({reason})"),
            };
            let _ = write!(out, "connection {}: {:?} {action}", c.name, c.status);
            if let Some(k) = &c.connection {
                let _ = write!(
                    out,
                    " engine={} type={} folder={:?} enabled={} reason={:?} read_only={:?} \
                     auto_commit={:?} schema={:?} marker={} init_sql={} tls={:?} ssh_hops={} \
                     props={:?} secrets={:?}",
                    k.engine,
                    k.connection_type,
                    k.folder,
                    k.enabled,
                    k.disabled_reason.as_ref().map(|r| r.kind),
                    k.read_only,
                    k.auto_commit,
                    k.default_schema,
                    k.init_sql_needs_confirmation,
                    k.init_sql.len(),
                    k.tls.mode,
                    k.ssh.as_ref().map_or(0, |s| s.jump_hosts.len() + 1),
                    k.properties.keys().collect::<Vec<_>>(),
                    c.secrets
                        .iter()
                        .map(|s| format!("{:?}:{:?}", s.slot, s.kind))
                        .collect::<Vec<_>>(),
                );
            }
            out.push('\n');
            let mut issues: Vec<String> = c
                .issues
                .iter()
                .map(|i| {
                    let line = format!(
                        "  - {:?} {:?} {}",
                        i.severity,
                        i.kind,
                        i.key.as_deref().unwrap_or("")
                    );
                    line.trim_end().to_owned()
                })
                .collect();
            issues.sort();
            issues.dedup();
            for i in issues {
                let _ = writeln!(out, "{i}");
            }
        }
    }
    out
}

fn check_snapshot(name: &str, actual: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/snapshots")
        .join(format!("{name}.txt"));
    if std::env::var_os("NINESQL_UPDATE_SNAPSHOTS").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_default();
    assert_eq!(
        actual, expected,
        "snapshot {name} differs (NINESQL_UPDATE_SNAPSHOTS=1 regenerates it; review the diff)"
    );
}

/// Exit criterion (phase 2): every P0-E fixture workspace previews and imports; snapshot of the
/// connections, folders, types and secrets moved (no value); the DBeaver files are
/// byte-identical (and untouched) after preview and apply (I4); no fixture secret reaches any
/// NineSQL file or the logs (I1).
#[test]
fn fixture_workspaces_import_with_snapshots_and_untouched_files() {
    for version in FIXTURE_VERSIONS {
        let e = env();
        let source = e.root.join("home/.local/share/DBeaverData/workspace6");
        copy_dir(&fixture(version), &source);
        let before = tree(&source);

        let capture = Capture::default();
        let outcome = tracing::subscriber::with_default(capture.clone(), || {
            let preview = e
                .importer
                .preview(&ImportSource::Detect(ninesql_dbeaver::SearchRoots {
                    home: e.root.join("home"),
                }))
                .unwrap();
            let text = summary(&preview);
            for secret in FIXTURE_SECRETS {
                assert!(!text.contains(secret));
                assert!(!format!("{preview:?}").contains(secret), "{version}");
            }
            check_snapshot(&format!("import-dbeaver-{version}"), &text);
            let outcome = e
                .importer
                .apply(&preview, &ImportChoices::all(Some(StoreKind::Vault)))
                .unwrap();
            assert_eq!(outcome.secrets_moved, preview.secret_count(), "{version}");
            assert!(outcome.secrets_moved >= 6, "{version}");
            assert_eq!(e.store.len(), outcome.secrets_moved);
            // The DBeaver workspace is a root of the session now (I4).
            assert!(
                e.roots
                    .roots()
                    .iter()
                    .any(|r| source.starts_with(r) || r == &source)
            );
            outcome
        });

        assert_eq!(tree(&source), before, "{version}: DBeaver files changed");
        assert_not_in_files(&e.root.join("ninesql"), FIXTURE_SECRETS);
        let logs = capture.0.lock().unwrap().clone();
        for secret in FIXTURE_SECRETS {
            assert!(
                !logs.contains(secret),
                "{version}: a secret reached the logs"
            );
        }
        assert!(
            outcome
                .report_file
                .starts_with(e.workspace.project_paths().dir())
        );
        let report =
            ninesql_storage::document::load::<ninesql_core::ImportReportFile>(&outcome.report_file)
                .unwrap()
                .unwrap();
        assert!(
            report.secret_like_keys.is_empty(),
            "{:?}",
            report.secret_like_keys
        );
        let moved: usize = report
            .document
            .projects
            .iter()
            .flat_map(|p| p.connections.iter())
            .map(|c| c.moved_to_store)
            .sum();
        assert_eq!(moved, outcome.secrets_moved);

        let connections = saved(&e.workspace);
        assert_eq!(connections.len(), outcome.created);
        let by_name = |n: &str| connections.iter().find(|c| c.name == n).unwrap();
        // Oracle is outside the v1 set: disabled for want of a driver.
        let oracle = by_name("oracle-legacy");
        assert!(!oracle.enabled);
        assert_eq!(
            oracle.disabled_reason.as_ref().unwrap().kind,
            DisabledKind::NoDriver
        );
        // pg-dev: init SQL marked, default schema kept, password in the store.
        let pg_dev = by_name("pg-dev");
        assert!(pg_dev.init_sql_needs_confirmation);
        assert_eq!(pg_dev.default_schema.as_deref(), Some("app"));
        assert_eq!(pg_dev.secret_refs().len(), 1);
        // mysql-test: manual commit kept as the connection's override.
        assert_eq!(by_name("mysql-test").auto_commit, Some(false));
        // The built-in Production type is only made stricter (DBeaver's confirm-execute),
        // keeping its name, color and production rules (review, M3).
        let types = e.workspace.load_connection_types().unwrap();
        let prod = types.get(&ConnectionTypeId::prod()).unwrap();
        let builtin = ninesql_core::ConnectionType::production();
        assert_eq!((&prod.name, prod.color), (&builtin.name, builtin.color));
        assert!(prod.is_production() && prod.confirm_destructive && !prod.auto_commit);
        assert!(prod.confirm_every_write);
        let test = types.get(&ConnectionTypeId::test()).unwrap();
        assert!(
            test.confirm_destructive,
            "DBeaver's confirm-data-change is kept"
        );
        assert!(
            types
                .get(&ConnectionTypeId::new("staging").unwrap())
                .is_some()
        );
    }
}

/// The repository's own fixture files are byte-identical after a preview and an apply run
/// straight on them (I4), not on a copy.
#[test]
fn preview_and_apply_on_the_repository_fixture_leave_it_byte_identical() {
    let e = env();
    let source = fixture("26.2.4");
    let before = tree(&source);
    let preview = e
        .importer
        .preview(&ImportSource::Folder(source.clone()))
        .unwrap();
    e.importer
        .apply(&preview, &ImportChoices::all(Some(StoreKind::Vault)))
        .unwrap();
    assert_eq!(tree(&source), before);
}

// ---------------------------------------------------------------------------------------------
// Marker

/// ADR-0022 §1 / P2-C (marker): a crafted "dev" connection with init SQL, imported, gives
/// `InitSqlNeedsConfirmation` with `imported = true`: before the save (test-before-save of the
/// preview), after it, and when the UI's copy lost the marker.
#[test]
fn an_imported_dev_connection_with_init_sql_needs_confirmation_end_to_end() {
    let tmp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(tmp.path()).unwrap();
    let source = crafted(
        &root,
        &format!(
            r#"{{ "connections": {{ {} }} }}"#,
            pg(
                "postgres-jdbc-crafted-1",
                "crafted-dev",
                &format!(
                    r#", "password": "{CRAFTED_PASSWORD}",
                       "bootstrap": {{ "query": ["SET search_path TO app"] }}"#
                ),
            )
        ),
    );
    let p = paths(&root.join("ninesql"));
    let mut config = CoreConfig::new(p.clone(), ProjectName::default_project(), registry());
    config.manager.known_hosts = KnownHostsFiles {
        own: p.data_dir().join("known_hosts"),
        user: None,
    };
    config.manager.workers = 2;
    config.manager.dbeaver_roots = DbeaverRoots::none().with_home(root.join("home"));
    config.use_secret_service = false;
    config.watch_desktop_lock = false;
    let capture = Capture::default();
    let _guard = tracing::subscriber::set_default(capture.clone());
    ui(async move {
        let services = tokio::time::timeout(Duration::from_secs(20), CoreServices::start(config))
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        services
            .create_vault_with(
                SecretString::from("Ma5ter-Pa55-6c0d2e".to_owned()),
                KdfParams::insecure_for_tests(),
            )
            .await
            .unwrap()
            .unwrap();
        let preview = services
            .preview_import(ImportSource::Folder(source.clone()))
            .await
            .unwrap()
            .unwrap();
        let types = services.workspace().load_connection_types().unwrap();
        let types = preview.types_after(&types);

        // Test-before-save: not on disk, the request's marker applies.
        let candidate = preview
            .connection_for_test("postgres-jdbc-crafted-1")
            .unwrap();
        assert!(candidate.init_sql_needs_confirmation);
        let outcome = support::open(services.sessions(), OpenRequest::new(candidate, &types))
            .await
            .unwrap();
        let OpenOutcome::InitSqlNeedsConfirmation(c) = outcome else {
            panic!("{outcome:?}");
        };
        assert!(c.imported);
        assert!(!c.production);

        let applied = services
            .apply_import(
                preview,
                ImportChoices::all(services.secret_backend().store()),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!((applied.created, applied.secrets_moved), (1, 1));
        let saved = services.workspace().load_connections().unwrap().connections;
        let mut c = saved.iter().next().unwrap().clone();
        assert!(c.init_sql_needs_confirmation);
        assert_eq!(c.connection_type, ConnectionTypeId::dev());
        assert_eq!(c.secret_refs()[0].2.store, StoreKind::Vault);
        let outcome = support::open(services.sessions(), OpenRequest::new(c.clone(), &types))
            .await
            .unwrap();
        assert!(matches!(outcome, OpenOutcome::InitSqlNeedsConfirmation(ref x) if x.imported));

        // A UI copy that lost the marker: the core reads it from disk.
        c.init_sql_needs_confirmation = false;
        let outcome = support::open(services.sessions(), OpenRequest::new(c, &types))
            .await
            .unwrap();
        assert!(matches!(outcome, OpenOutcome::InitSqlNeedsConfirmation(ref x) if x.imported));
        services
    })
    .shutdown(Duration::from_secs(5));
    assert_not_in_files(&root.join("ninesql"), &[CRAFTED_PASSWORD]);
    assert!(!capture.0.lock().unwrap().contains(CRAFTED_PASSWORD));
}

/// The marker is NineSQL's: a DBeaver file cannot clear it, and a re-import sets it again
/// even after the user confirmed (and cleared) it.
#[test]
fn reimport_sets_the_marker_again_and_keeps_user_edits() {
    let e = env();
    let json = format!(
        r#"{{ "connections": {{ {} , {} }} }}"#,
        pg(
            "postgres-jdbc-a",
            "a",
            r#", "init_sql_needs_confirmation": false, "bootstrap": { "query": ["SET search_path TO app"] }"#
        ),
        pg("postgres-jdbc-b", "b", "")
    );
    let source = crafted(&e.root, &json);
    let preview = e
        .importer
        .preview(&ImportSource::Folder(source.clone()))
        .unwrap();
    assert!(
        plan(&preview, "a")
            .connection
            .as_ref()
            .unwrap()
            .init_sql_needs_confirmation
    );
    e.importer
        .apply(&preview, &ImportChoices::all(Some(StoreKind::Vault)))
        .unwrap();

    // The user confirms the init SQL, renames `a` and moves it to a folder.
    let mut connections = saved(&e.workspace);
    let mut a = connections.iter().find(|c| c.name == "a").unwrap().clone();
    let id = a.id;
    assert!(
        e.workspace
            .clear_init_sql_marker(id, &a.init_sql)
            .unwrap()
            .is_clear()
    );
    a.name = "renamed".into();
    a.folder = vec!["Mine".into()];
    a.init_sql_needs_confirmation = false;
    e.workspace
        .update_connection(&mut connections, a, &registry())
        .unwrap();

    let again = e.importer.preview(&ImportSource::Folder(source)).unwrap();
    assert!(
        matches!(plan(&again, "a").action, PlanAction::Update { existing, .. } if existing == id)
    );
    let outcome = e
        .importer
        .apply(&again, &ImportChoices::all(Some(StoreKind::Vault)))
        .unwrap();
    assert_eq!((outcome.created, outcome.updated), (0, 2));
    let connections = saved(&e.workspace);
    assert_eq!(connections.len(), 2, "no duplicate");
    let a = connections.get(id).unwrap();
    assert_eq!(a.name, "renamed");
    assert_eq!(a.folder, ["Mine"]);
    assert!(
        a.init_sql_needs_confirmation,
        "re-applied init SQL is marked again"
    );
}

/// `update_connection` and `save_connections_checked` keep a marker the caller's copy lost.
#[test]
fn ui_writes_keep_the_marker_of_unchanged_init_sql() {
    let e = env();
    let source = crafted(
        &e.root,
        &format!(
            r#"{{ "connections": {{ {} }} }}"#,
            pg(
                "postgres-jdbc-m",
                "m",
                r#", "bootstrap": { "query": ["SET search_path TO app"] }"#
            )
        ),
    );
    let preview = e.importer.preview(&ImportSource::Folder(source)).unwrap();
    e.importer
        .apply(&preview, &ImportChoices::all(None))
        .unwrap();
    let mut connections = saved(&e.workspace);
    let mut c = connections.iter().next().unwrap().clone();
    let id = c.id;
    c.init_sql_needs_confirmation = false;
    c.description = Some("edited".into());
    e.workspace
        .update_connection(&mut connections, c.clone(), &registry())
        .unwrap();
    assert!(
        saved(&e.workspace)
            .get(id)
            .unwrap()
            .init_sql_needs_confirmation
    );

    let mut list = Connections::from_vec(vec![c]).unwrap();
    list.get_mut(id).unwrap().init_sql_needs_confirmation = false;
    e.workspace
        .save_connections_checked(&list, &registry())
        .unwrap();
    assert!(
        saved(&e.workspace)
            .get(id)
            .unwrap()
            .init_sql_needs_confirmation
    );
}

// ---------------------------------------------------------------------------------------------
// Disabled reasons, types, properties, files

/// A connection disabled because its route could not be imported (a proxy handler) keeps its
/// reason: the UI write paths refuse to drop it, and enabling needs the route confirmed.
#[test]
fn a_route_disabled_connection_cannot_simply_be_re_enabled() {
    let e = env();
    let source = crafted(
        &e.root,
        &format!(
            r#"{{ "connections": {{ {} }} }}"#,
            pg(
                "postgres-jdbc-proxy",
                "via-proxy",
                r#", "handlers": { "proxy_http": { "type": "PROXY", "enabled": true,
                     "properties": { "host": "proxy.example.invalid", "port": "3128" } } }"#
            )
        ),
    );
    let preview = e.importer.preview(&ImportSource::Folder(source)).unwrap();
    assert_eq!(
        plan(&preview, "via-proxy").status,
        ImportStatus::ImportedDisabled
    );
    e.importer
        .apply(&preview, &ImportChoices::all(None))
        .unwrap();
    let mut connections = saved(&e.workspace);
    let c = connections.iter().next().unwrap().clone();
    let id = c.id;
    assert!(!c.enabled);
    assert_eq!(
        c.disabled_reason.as_ref().unwrap().kind,
        DisabledKind::Route
    );

    // Dropping the reason through the UI paths is refused.
    let mut forced = c.clone();
    forced.enabled = true;
    forced.disabled_reason = None;
    assert!(
        e.workspace
            .update_connection(&mut connections, forced.clone(), &registry())
            .is_err()
    );
    let list = Connections::from_vec(vec![forced]).unwrap();
    assert!(
        e.workspace
            .save_connections_checked(&list, &registry())
            .is_err()
    );
    // Keeping it while enabled is invalid.
    let mut half = c.clone();
    half.enabled = true;
    assert!(half.validate().is_err());

    // Enabling without confirming the route is refused; with it, allowed.
    assert!(
        e.workspace
            .enable_connection(&mut connections, id, false, &registry())
            .is_err()
    );
    assert!(!saved(&e.workspace).get(id).unwrap().enabled);
    e.workspace
        .enable_connection(&mut connections, id, true, &registry())
        .unwrap();
    let c = saved(&e.workspace);
    let c = c.get(id).unwrap();
    assert!(c.enabled && c.disabled_reason.is_none());
}

/// A crafted file cannot relax the built-in types nor a saved custom type; a connection whose
/// re-imported type is weaker keeps its saved type.
#[test]
fn imported_types_never_weaken_existing_ones() {
    let e = env();
    let mut strict = ninesql_core::ConnectionType::development();
    strict.id = ConnectionTypeId::new("staging").unwrap();
    strict.name = "Staging".into();
    strict.auto_commit = false;
    strict.confirm_destructive = true;
    let mut types = e.workspace.load_connection_types().unwrap();
    types.upsert(strict.clone());
    e.workspace.save_connection_types(&types).unwrap();

    let json = format!(
        r#"{{ "connections": {{ {} }},
             "connection-types": {{
               "prod": {{ "name": "Production", "color": "1,2,3", "auto-commit": true,
                          "confirm-execute": false, "confirm-data-change": false }},
               "staging": {{ "name": "Staging", "color": "1,2,3", "auto-commit": true,
                             "confirm-execute": false, "confirm-data-change": false }} }} }}"#,
        pg("postgres-jdbc-t", "t", "").replace(r#""type": "dev""#, r#""type": "prod""#)
    );
    let source = crafted(&e.root, &json);
    let preview = e
        .importer
        .preview(&ImportSource::Folder(source.clone()))
        .unwrap();
    e.importer
        .apply(&preview, &ImportChoices::all(None))
        .unwrap();
    let types = e.workspace.load_connection_types().unwrap();
    assert_eq!(
        types.get(&ConnectionTypeId::prod()).unwrap(),
        &ninesql_core::ConnectionType::production()
    );
    assert_eq!(types.get(&strict.id).unwrap(), &strict);

    // Re-import with the connection typed `dev` (weaker than its saved `prod`): kept `prod`.
    std::fs::write(
        source.join("General/.dbeaver/data-sources.json"),
        format!(
            r#"{{ "connections": {{ {} }} }}"#,
            pg("postgres-jdbc-t", "t", "")
        ),
    )
    .unwrap();
    let preview = e.importer.preview(&ImportSource::Folder(source)).unwrap();
    e.importer
        .apply(&preview, &ImportChoices::all(None))
        .unwrap();
    let c = saved(&e.workspace);
    assert_eq!(
        c.iter().next().unwrap().connection_type,
        ConnectionTypeId::prod()
    );
}

/// pgjdbc properties without an equivalent never reach the saved connection (pgwire refuses
/// them at connect), each with a report entry; the connection stays enabled.
#[test]
fn driver_properties_without_an_equivalent_are_dropped_and_reported() {
    let e = env();
    let source = crafted(
        &e.root,
        &format!(
            r#"{{ "connections": {{ {} }} }}"#,
            pg(
                "postgres-jdbc-p",
                "p",
                r#", "properties": { "prepareThreshold": "5", "ApplicationName": "x",
                                     "defaultRowFetchSize": "100" }"#
            )
        ),
    );
    let preview = e.importer.preview(&ImportSource::Folder(source)).unwrap();
    let p = plan(&preview, "p");
    let c = p.connection.as_ref().unwrap();
    assert!(c.enabled, "{:?}", c.disabled_reason);
    for key in ["prepareThreshold", "ApplicationName", "defaultRowFetchSize"] {
        assert!(!c.properties.contains_key(key), "{key}");
        assert!(
            p.issues
                .iter()
                .any(|i| i.key.as_deref().is_some_and(|k| k.ends_with(key))),
            "{key} has no report entry"
        );
    }
    registry().validate_connection(c).unwrap();
}

/// A SQLite file inside the DBeaver workspace is imported read-only through `DbeaverRoots`,
/// and the workspace is a root of the session from the preview on (I4).
#[test]
fn a_sqlite_file_inside_the_dbeaver_workspace_is_read_only() {
    let e = env();
    let ws = e.root.join("dbeaver").join("workspace6");
    let db = ws.join("General").join("local.sqlite");
    let source = crafted(
        &e.root,
        &format!(
            r#"{{ "connections": {{ "sqlite_jdbc-x": {{ "provider": "sqlite", "driver": "sqlite_jdbc",
                 "name": "inside", "configuration": {{ "database": "{}", "type": "dev" }} }} }} }}"#,
            db.display()
        ),
    );
    std::fs::write(&db, b"").unwrap();
    let preview = e
        .importer
        .preview(&ImportSource::Folder(source.clone()))
        .unwrap();
    let c = plan(&preview, "inside").connection.clone().unwrap();
    assert_eq!(c.read_only, Some(true));
    assert!(c.enabled);
    assert!(e.roots.contains(&db));
    assert!(e.roots.roots().contains(&source));
}

/// The source changed between preview and apply: nothing is written.
#[test]
fn a_source_changed_since_the_preview_is_refused() {
    let e = env();
    let source = crafted(
        &e.root,
        &format!(
            r#"{{ "connections": {{ {} }} }}"#,
            pg("postgres-jdbc-s", "s", "")
        ),
    );
    let preview = e
        .importer
        .preview(&ImportSource::Folder(source.clone()))
        .unwrap();
    std::fs::write(
        source.join("General/.dbeaver/data-sources.json"),
        format!(
            r#"{{ "connections": {{ {} , {} }} }}"#,
            pg("postgres-jdbc-s", "s", ""),
            pg("postgres-jdbc-s2", "s2", "")
        ),
    )
    .unwrap();
    let err = e
        .importer
        .apply(&preview, &ImportChoices::all(None))
        .unwrap_err();
    assert!(matches!(err, ImportError::SourceChanged), "{err:?}");
    assert!(saved(&e.workspace).is_empty());
}

/// Skipped connections and "no store": nothing stored, the fields ask at connect.
#[test]
fn choices_skip_connections_and_can_leave_secrets_behind() {
    let e = env();
    let pw = format!(r#", "password": "{CRAFTED_PASSWORD}""#);
    let source = crafted(
        &e.root,
        &format!(
            r#"{{ "connections": {{ {} , {} }} }}"#,
            pg("postgres-jdbc-k", "keep", &pw),
            pg("postgres-jdbc-x", "skip", &pw)
        ),
    );
    let preview = e.importer.preview(&ImportSource::Folder(source)).unwrap();
    assert_eq!(plan(&preview, "keep").secrets[0].slot, SecretSlot::Auth);
    let mut choices = ImportChoices::all(None);
    choices.skip.insert("postgres-jdbc-x".into());
    let outcome = e.importer.apply(&preview, &choices).unwrap();
    assert_eq!(
        (outcome.created, outcome.skipped, outcome.secrets_moved),
        (1, 1, 0)
    );
    assert!(e.store.is_empty());
    let c = saved(&e.workspace);
    assert!(c.iter().next().unwrap().secret_refs().is_empty());
    assert_not_in_files(&e.root.join("ninesql"), &[CRAFTED_PASSWORD]);
}

/// The vault is the chosen store and it is locked: nothing is written; unlock and apply again.
#[test]
fn a_locked_vault_stops_the_import_before_any_write() {
    let tmp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(tmp.path()).unwrap();
    let source = crafted(
        &root,
        &format!(
            r#"{{ "connections": {{ {} }} }}"#,
            pg(
                "postgres-jdbc-v",
                "v",
                &format!(r#", "password": "{CRAFTED_PASSWORD}""#)
            )
        ),
    );
    let p = paths(&root.join("ninesql"));
    let mut config = CoreConfig::new(p.clone(), ProjectName::default_project(), registry());
    config.manager.known_hosts = KnownHostsFiles {
        own: p.data_dir().join("known_hosts"),
        user: None,
    };
    config.manager.workers = 2;
    config.manager.dbeaver_roots = DbeaverRoots::none().with_home(root.join("home"));
    config.use_secret_service = false;
    config.watch_desktop_lock = false;
    ui(async move {
        let services = CoreServices::start(config).await.unwrap().unwrap();
        let master = || SecretString::from("Ma5ter-Pa55-6c0d2e".to_owned());
        let early = services
            .preview_import(ImportSource::Folder(source.clone()))
            .await
            .unwrap()
            .unwrap();
        let err = services
            .apply_import(early, ImportChoices::all(Some(StoreKind::Vault)))
            .await
            .unwrap()
            .unwrap_err();
        assert!(matches!(err, ImportError::VaultNotCreated), "{err:?}");
        services
            .create_vault_with(master(), KdfParams::insecure_for_tests())
            .await
            .unwrap()
            .unwrap();
        services.lock_now().await.unwrap();
        let preview = services
            .preview_import(ImportSource::Folder(source))
            .await
            .unwrap()
            .unwrap();
        let err = services
            .apply_import(preview.clone(), ImportChoices::all(Some(StoreKind::Vault)))
            .await
            .unwrap()
            .unwrap_err();
        assert!(matches!(err, ImportError::VaultLocked), "{err:?}");
        assert!(
            services
                .workspace()
                .load_connections()
                .unwrap()
                .connections
                .is_empty()
        );
        services.unlock_vault(master()).await.unwrap().unwrap();
        let outcome = services
            .apply_import(preview, ImportChoices::all(Some(StoreKind::Vault)))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(outcome.secrets_moved, 1);
        services
    })
    .shutdown(Duration::from_secs(5));
    assert_not_in_files(&root.join("ninesql"), &[CRAFTED_PASSWORD]);
}

/// Exit criterion (phase 2): a generated 100-connection workspace previews and imports in
/// under 2 s. Method: 100 PostgreSQL connections with a plaintext password each (moved to an
/// in-memory store), init SQL and properties, preview + apply timed together in this test
/// process (debug or release profile); the bound holds in the debug profile on CI machines.
#[test]
fn a_100_connection_workspace_imports_in_under_two_seconds() {
    let e = env();
    let connections: Vec<String> = (0..100)
        .map(|i| {
            pg(
                &format!("postgres-jdbc-gen-{i:03}"),
                &format!("gen-{i:03}"),
                &format!(
                    r#", "password": "gen-pw-{i:03}-x9", "folder": "F{}",
                       "bootstrap": {{ "query": ["SET search_path TO app"] }},
                       "properties": {{ "connectTimeout": "5" }}"#,
                    i % 7
                ),
            )
        })
        .collect();
    let source = crafted(
        &e.root,
        &format!(r#"{{ "connections": {{ {} }} }}"#, connections.join(",")),
    );
    let start = Instant::now();
    let preview = e.importer.preview(&ImportSource::Folder(source)).unwrap();
    let outcome = e
        .importer
        .apply(&preview, &ImportChoices::all(Some(StoreKind::Vault)))
        .unwrap();
    let elapsed = start.elapsed();
    assert_eq!(outcome.created, 100);
    assert_eq!(outcome.secrets_moved, 100);
    assert!(elapsed < Duration::from_secs(2), "took {elapsed:?}");
    assert_not_in_files(&e.root.join("ninesql"), &["gen-pw-042-x9"]);
}

/// A re-import replaces the stored value of a secret DBeaver saved (same reference), and keeps
/// a secret saved in NineSQL for a field DBeaver has no value for.
#[test]
fn reimport_replaces_imported_secrets_and_keeps_secrets_saved_in_ninesql() {
    use ninesql_secrets::{ExposeSecret, SecretStore};
    let e = env();
    let with_pw = |pw: &str| {
        format!(
            r#"{{ "connections": {{ {} }} }}"#,
            pg("postgres-jdbc-r", "r", &format!(r#", "password": "{pw}""#))
        )
    };
    let source = crafted(&e.root, &with_pw("First-Pa55-0001"));
    let preview = e
        .importer
        .preview(&ImportSource::Folder(source.clone()))
        .unwrap();
    e.importer
        .apply(&preview, &ImportChoices::all(Some(StoreKind::Vault)))
        .unwrap();
    let id = saved(&e.workspace).iter().next().unwrap().id;
    let first = saved(&e.workspace).get(id).unwrap().secret_refs()[0]
        .2
        .clone();

    std::fs::write(
        source.join("General/.dbeaver/data-sources.json"),
        with_pw("Second-Pa55-0002"),
    )
    .unwrap();
    let preview = e
        .importer
        .preview(&ImportSource::Folder(source.clone()))
        .unwrap();
    e.importer
        .apply(&preview, &ImportChoices::all(Some(StoreKind::Vault)))
        .unwrap();
    // Written under a fresh reference; the replaced one is deleted after the write (M1).
    let second = saved(&e.workspace).get(id).unwrap().secret_refs()[0]
        .2
        .clone();
    assert_ne!(first, second);
    assert!(second.id.as_str().starts_with(&format!("{id}:auth:")));
    assert_eq!(e.store.len(), 1);
    assert_eq!(
        e.store.get(&second.id).unwrap().expose_secret(),
        "Second-Pa55-0002"
    );

    // DBeaver no longer saves the password; the one in the store stays referenced.
    std::fs::write(
        source.join("General/.dbeaver/data-sources.json"),
        format!(
            r#"{{ "connections": {{ {} }} }}"#,
            pg("postgres-jdbc-r", "r", "")
        ),
    )
    .unwrap();
    let preview = e.importer.preview(&ImportSource::Folder(source)).unwrap();
    assert!(plan(&preview, "r").secrets.is_empty());
    e.importer
        .apply(&preview, &ImportChoices::all(Some(StoreKind::Vault)))
        .unwrap();
    let c = saved(&e.workspace);
    assert_eq!(c.get(id).unwrap().secret_refs().len(), 1);
    assert_eq!(e.store.len(), 1);
    assert_not_in_files(
        &e.root.join("ninesql"),
        &["First-Pa55-0001", "Second-Pa55-0002"],
    );
}

fn rewrite(source: &Path, connections: &[String]) {
    std::fs::write(
        source.join("General/.dbeaver/data-sources.json"),
        format!(r#"{{ "connections": {{ {} }} }}"#, connections.join(",")),
    )
    .unwrap();
}

fn import_all(e: &Env, source: &Path, store: Option<StoreKind>) -> ImportPreview {
    let preview = e
        .importer
        .preview(&ImportSource::Folder(source.to_path_buf()))
        .unwrap();
    e.importer
        .apply(&preview, &ImportChoices::all(store))
        .unwrap();
    preview
}

/// Review H1 (a): another DBeaver workspace reusing the id never updates the saved connection,
/// whose name, host and stored password stay as they were; the match is reported.
#[test]
fn a_foreign_workspace_with_the_same_dbeaver_id_never_updates_a_connection() {
    let e = env();
    let real = crafted(
        &e.root.join("real"),
        &format!(
            r#"{{ "connections": {{ {} }} }}"#,
            pg(
                "postgres-jdbc-bill",
                "billing",
                r#", "password": "Real-Pa55-0042""#
            )
        ),
    );
    import_all(&e, &real, Some(StoreKind::Vault));
    let mut connections = saved(&e.workspace);
    let mut c = connections.iter().next().unwrap().clone();
    let id = c.id;
    c.name = "Prod billing".into();
    e.workspace
        .update_connection(&mut connections, c, &registry())
        .unwrap();
    let before = saved(&e.workspace).get(id).unwrap().clone();

    let foreign = crafted(
        &e.root.join("foreign"),
        &format!(
            r#"{{ "connections": {{ {} }} }}"#,
            pg("postgres-jdbc-bill", "billing", "")
                .replace("db.example.invalid", "evil.example.invalid")
        ),
    );
    let preview = e
        .importer
        .preview(&ImportSource::Folder(foreign.clone()))
        .unwrap();
    let p = plan(&preview, "billing");
    assert_eq!(p.action, PlanAction::Create);
    let new = p.connection.as_ref().unwrap();
    assert_ne!(new.id, id);
    assert!(new.secret_refs().is_empty());
    assert!(
        p.issues
            .iter()
            .any(|i| i.message.contains("same DBeaver id"))
    );
    e.importer
        .apply(&preview, &ImportChoices::all(Some(StoreKind::Vault)))
        .unwrap();
    let after = saved(&e.workspace);
    assert_eq!(after.len(), 2);
    assert_eq!(
        after.get(id).unwrap(),
        &before,
        "the saved connection is untouched"
    );
}

/// Review H1 (b): a re-import that moves the endpoint drops the saved password (and deletes
/// it after the write) instead of sending it to the new host; the diff says so.
#[test]
fn a_reimport_to_another_host_drops_the_saved_password() {
    let e = env();
    let source = crafted(
        &e.root,
        &format!(
            r#"{{ "connections": {{ {} }} }}"#,
            pg("postgres-jdbc-h", "h", r#", "password": "Host-Pa55-0007""#)
        ),
    );
    import_all(&e, &source, Some(StoreKind::Vault));
    assert_eq!(e.store.len(), 1);
    rewrite(
        &source,
        &[pg("postgres-jdbc-h", "h", "").replace("db.example.invalid", "evil.example.invalid")],
    );
    let preview = e
        .importer
        .preview(&ImportSource::Folder(source.clone()))
        .unwrap();
    let p = plan(&preview, "h");
    let PlanAction::Update { diff, .. } = &p.action else {
        panic!("{:?}", p.action);
    };
    assert!(diff.endpoint.is_some());
    assert_eq!(
        diff.secrets,
        [SecretChange {
            slot: SecretSlot::Auth,
            change: SecretChangeKind::Dropped
        }]
    );
    assert!(p.connection.as_ref().unwrap().secret_refs().is_empty());
    e.importer
        .apply(&preview, &ImportChoices::all(Some(StoreKind::Vault)))
        .unwrap();
    assert!(
        saved(&e.workspace)
            .iter()
            .next()
            .unwrap()
            .secret_refs()
            .is_empty()
    );
    assert!(e.store.is_empty(), "the dropped secret is deleted");
}

/// Review H1 (c, e): a re-import with a weaker TLS mode keeps the saved mode and password
/// unless the user accepts the change, which then drops the password; the test-before-save
/// connection never carries a saved secret.
#[test]
fn a_weaker_tls_on_reimport_is_kept_unless_accepted() {
    let e = env();
    let source = crafted(
        &e.root,
        &format!(
            r#"{{ "connections": {{ {} }} }}"#,
            pg("postgres-jdbc-t", "t", r#", "password": "Tls-Pa55-0009""#)
        ),
    );
    import_all(&e, &source, Some(StoreKind::Vault));
    let weak = pg(
        "postgres-jdbc-t",
        "t",
        r#", "handlers": { "postgre_ssl": { "type": "CONFIG", "enabled": true,
             "properties": { "sslMode": "disable" } } }"#,
    );
    rewrite(&source, &[weak]);
    let preview = e
        .importer
        .preview(&ImportSource::Folder(source.clone()))
        .unwrap();
    let p = plan(&preview, "t");
    let PlanAction::Update { diff, .. } = &p.action else {
        panic!("{:?}", p.action);
    };
    let tls = diff.tls.as_ref().unwrap();
    assert!(tls.kept_saved);
    assert_eq!(tls.imported.0, TlsMode::Disable);
    let c = p.connection.as_ref().unwrap();
    assert_eq!(c.tls.mode, TlsMode::VerifyFull);
    assert_eq!(c.secret_refs().len(), 1, "same server, same TLS: kept");
    // (e) The test-before-save copy asks for every credential, marker kept.
    let test = preview.connection_for_test("postgres-jdbc-t").unwrap();
    assert!(test.secret_refs().is_empty());

    // Default: saved TLS and password kept.
    e.importer
        .apply(&preview, &ImportChoices::all(Some(StoreKind::Vault)))
        .unwrap();
    let c = saved(&e.workspace).iter().next().unwrap().clone();
    assert_eq!(c.tls.mode, TlsMode::VerifyFull);
    assert_eq!(c.secret_refs().len(), 1);

    // Accepted: the weaker mode applies and the password no longer does.
    let preview = e
        .importer
        .preview(&ImportSource::Folder(source.clone()))
        .unwrap();
    let mut choices = ImportChoices::all(Some(StoreKind::Vault));
    choices.accept_tls_changes.insert("postgres-jdbc-t".into());
    e.importer.apply(&preview, &choices).unwrap();
    let c = saved(&e.workspace).iter().next().unwrap().clone();
    assert_eq!(c.tls.mode, TlsMode::Disable);
    assert!(c.secret_refs().is_empty());
    assert!(e.store.is_empty());
}

/// Review L1: a re-import with no store keeps the password saved earlier (same server).
#[test]
fn a_reimport_without_a_store_keeps_the_saved_password() {
    let e = env();
    let source = crafted(
        &e.root,
        &format!(
            r#"{{ "connections": {{ {} }} }}"#,
            pg("postgres-jdbc-n", "n", r#", "password": "Keep-Pa55-0011""#)
        ),
    );
    import_all(&e, &source, Some(StoreKind::Vault));
    import_all(&e, &source, None);
    assert_eq!(
        saved(&e.workspace)
            .iter()
            .next()
            .unwrap()
            .secret_refs()
            .len(),
        1
    );
    assert_eq!(e.store.len(), 1);
}

/// Review L2: a connection edited since the preview is not overwritten.
#[test]
fn an_edit_since_the_preview_stops_the_apply() {
    let e = env();
    let source = crafted(
        &e.root,
        &format!(
            r#"{{ "connections": {{ {} }} }}"#,
            pg("postgres-jdbc-l", "l", "")
        ),
    );
    import_all(&e, &source, None);
    let preview = e
        .importer
        .preview(&ImportSource::Folder(source.clone()))
        .unwrap();
    let mut connections = saved(&e.workspace);
    let mut c = connections.iter().next().unwrap().clone();
    c.name = "edited meanwhile".into();
    e.workspace
        .update_connection(&mut connections, c, &registry())
        .unwrap();
    let err = e
        .importer
        .apply(&preview, &ImportChoices::all(None))
        .unwrap_err();
    assert!(matches!(err, ImportError::SourceChanged), "{err:?}");
    assert_eq!(
        saved(&e.workspace).iter().next().unwrap().name,
        "edited meanwhile"
    );
}

/// Review M4a: a route reason cannot be turned into another reason through the UI paths
/// (an `invalid` reason would be enabled without confirming the route).
#[test]
fn a_disabled_reason_cannot_be_changed_through_the_ui_paths() {
    let e = env();
    let source = crafted(
        &e.root,
        &format!(
            r#"{{ "connections": {{ {} }} }}"#,
            pg(
                "postgres-jdbc-r2",
                "r2",
                r#", "handlers": { "proxy_http": { "type": "PROXY", "enabled": true,
                     "properties": { "host": "proxy.example.invalid", "port": "3128" } } }"#
            )
        ),
    );
    import_all(&e, &source, None);
    let mut connections = saved(&e.workspace);
    let mut c = connections.iter().next().unwrap().clone();
    let id = c.id;
    c.disabled_reason = Some(ninesql_core::DisabledReason::new(
        DisabledKind::Invalid,
        "x",
    ));
    assert!(
        e.workspace
            .update_connection(&mut connections, c.clone(), &registry())
            .is_err()
    );
    let list = Connections::from_vec(vec![c]).unwrap();
    assert!(
        e.workspace
            .save_connections_checked(&list, &registry())
            .is_err()
    );
    let kept = saved(&e.workspace);
    assert_eq!(
        kept.get(id).unwrap().disabled_reason.as_ref().unwrap().kind,
        DisabledKind::Route
    );
    assert!(
        e.workspace
            .enable_connection(&mut connections, id, false, &registry())
            .is_err()
    );
}

/// Review L4: a property kept on a connection without a driver is checked again at enable.
#[test]
fn properties_of_a_connection_without_a_driver_are_checked_at_enable() {
    let tmp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(tmp.path()).unwrap();
    let workspace =
        Workspace::open(paths(&root.join("ninesql")), ProjectName::default_project()).unwrap();
    let mut only_pg = DriverRegistry::new();
    only_pg.register(Arc::new(ninesql_driver_pgwire::PgWireDriver::new()));
    let importer = Importer::new(
        workspace.clone(),
        only_pg,
        SecretStores::new(),
        DbeaverRoots::none().with_home(root.join("home")),
        None,
    );
    let db = root.join("x.sqlite");
    let source = crafted(
        &root,
        &format!(
            r#"{{ "connections": {{ "sqlite_jdbc-p": {{ "provider": "sqlite", "driver": "sqlite_jdbc",
                 "name": "p", "configuration": {{ "database": "{}", "type": "dev",
                 "properties": {{ "unvetted_option": "1" }} }} }} }} }}"#,
            db.display()
        ),
    );
    let preview = importer.preview(&ImportSource::Folder(source)).unwrap();
    importer.apply(&preview, &ImportChoices::all(None)).unwrap();
    let mut connections = saved(&workspace);
    let c = connections.iter().next().unwrap().clone();
    assert_eq!(
        c.disabled_reason.as_ref().unwrap().kind,
        DisabledKind::NoDriver
    );
    assert!(c.properties.contains_key("unvetted_option"));
    {
        // With the driver registered, enabling re-checks the property against its allowlist.
        assert!(
            workspace
                .enable_connection(&mut connections, c.id, false, &registry())
                .is_err()
        );
    }
}
