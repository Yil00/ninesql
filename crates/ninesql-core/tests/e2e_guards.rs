//! Test-build guards of the scenario runner (feature `e2e-scenarios`, ADR-0029 §2.5), through
//! the public API the UI runner uses: before `install` everything is refused; after it, only
//! loopback endpoints (checked after resolution, the checked address being the one connected
//! to), database files, export and import files inside the run directory, and the vault.
//!
//! Run with `cargo test -p ninesql-core --features e2e-scenarios --test e2e_guards`. One test per
//! process: the guards are process-wide. (With the feature on, the other test binaries of the
//! crate fail closed: they connect without installing the guards.)
#![cfg(feature = "e2e-scenarios")]
#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).

mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use ninesql_core::dbeaver::export::ExportTarget;
use ninesql_core::e2e::{self, GuardError};
use ninesql_core::transfer::{CsvPreviewRequest, Overwrite, TargetColumn, TransferError};
use ninesql_core::{
    ConnectionTypeId, ConnectionTypes, CoreConfig, CoreError, CoreServices, DbeaverExportError,
    DriverRegistry, Endpoint, HostKeyPolicy, ImportError, ImportSource, KnownHostsFiles,
    OpenOutcome, OpenRequest, SecretBackend, SshAuth, SshHop, SshTunnel, StartupError, TlsMode,
};
use ninesql_storage::{Paths, ProjectName, XdgDirs};
use support::*;

fn private_dir() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let canonical = std::fs::canonicalize(tmp.path()).unwrap();
    (tmp, canonical)
}

fn config(root: &Path, wrap: &Wrap) -> CoreConfig {
    let paths = Paths::new(&XdgDirs {
        config_home: root.join("config"),
        data_home: root.join("data"),
        state_home: root.join("state"),
    });
    let mut registry = DriverRegistry::new();
    registry.register(Arc::new(wrap.clone()));
    registry.register(Arc::new(ninesql_driver_sqlite::SqliteDriver::new()));
    let mut c = CoreConfig::new(paths.clone(), ProjectName::default_project(), registry);
    c.manager.known_hosts = KnownHostsFiles {
        own: paths.data_dir().join("known_hosts"),
        user: None,
    };
    c.manager.workers = 2;
    c.manager.connect_timeout = Duration::from_secs(5);
    c.manager.dbeaver_roots = ninesql_core::DbeaverRoots::none();
    c.watch_desktop_lock = false;
    // Left on: a test build must not probe the Secret Service whatever the caller asks.
    c.use_secret_service = true;
    c
}

fn start(c: CoreConfig) -> Result<CoreServices, StartupError> {
    ui(async {
        tokio::time::timeout(Duration::from_secs(20), CoreServices::start(c))
            .await
            .unwrap()
            .unwrap()
    })
}

fn open_at(services: &CoreServices, host: &str) -> Result<OpenOutcome, CoreError> {
    let mut c = mock_connection(ConnectionTypeId::dev());
    c.endpoint = Endpoint::Tcp {
        host: host.into(),
        port: Some(5000),
        database: None,
    };
    c.tls.mode = TlsMode::Disable;
    ui(open(
        services.sessions(),
        OpenRequest::new(c, &ConnectionTypes::default()),
    ))
}

fn refused_by(result: Result<OpenOutcome, CoreError>) -> GuardError {
    match result {
        Err(CoreError::TestBuild(e)) => e,
        other => panic!("not refused by a test-build guard: {other:?}"),
    }
}

#[test]
fn test_build_guards_confine_connections_files_and_secrets() {
    let (_tmp, run) = private_dir();
    if cfg!(feature = "secret-service") {
        // `--all-features`: a test build with the keyring backend refuses to run at all.
        assert_eq!(e2e::install(&run), Err(GuardError::SecretServiceCompiledIn));
        assert_eq!(e2e::installed(), Err(GuardError::NotInstalled));
        return;
    }
    let wrap = Wrap::new(mock_driver());

    // Before `install`: the core does not start, the registry's drivers refuse to connect.
    assert!(matches!(
        start(config(&run, &wrap)),
        Err(StartupError::TestBuild(GuardError::NotInstalled))
    ));
    let mut registry = DriverRegistry::new();
    registry.register(Arc::new(wrap.clone()));
    let driver = registry.driver(&mock_engine()).unwrap();
    let refused = ui(driver.connect(ninesql_core::driver_api::ConnectParams::new(
        ninesql_core::driver_api::EngineId::new(
            ninesql_core::driver_api::mock::MOCK_ENGINE.as_str(),
        ),
        ninesql_core::driver_api::Endpoint::tcp("127.0.0.1", 5000),
    )));
    let message = refused.err().map(|e| e.to_string()).unwrap_or_default();
    assert!(
        message.contains("not installed"),
        "not installed: refused ({message})"
    );
    assert!(wrap.seen.lock().unwrap().is_empty());

    // `install`: a bad run directory is refused, the run directory once, another one never.
    let (_other_tmp, other) = private_dir();
    std::fs::set_permissions(&other, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(matches!(
        e2e::install(&other),
        Err(GuardError::RunDirectory(_))
    ));
    std::fs::set_permissions(&other, std::fs::Permissions::from_mode(0o700)).unwrap();
    let guards = e2e::install(&run).unwrap();
    assert_eq!(guards.run_dir(), run);
    assert_eq!(e2e::install(&run).unwrap(), guards);
    assert_eq!(e2e::install(&other), Err(GuardError::AlreadyInstalled));

    // The second layer: a registered driver called directly takes loopback IP literals only.
    let params = |host: &str| {
        let mut p = ninesql_core::driver_api::ConnectParams::new(
            ninesql_core::driver_api::EngineId::new(
                ninesql_core::driver_api::mock::MOCK_ENGINE.as_str(),
            ),
            ninesql_core::driver_api::Endpoint::tcp(host, 5000),
        );
        p.tls.mode = ninesql_core::driver_api::TlsMode::Disable;
        p
    };
    for host in ["localhost", "db.example.test", "10.0.0.1"] {
        let message = ui(driver.connect(params(host)))
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(
            message.contains("not a loopback endpoint"),
            "{host}: {message}"
        );
    }
    assert!(wrap.seen.lock().unwrap().is_empty());
    assert!(ui(driver.connect(params("127.0.0.1"))).is_ok());
    assert_eq!(wrap.seen.lock().unwrap().len(), 1);

    // NineSQL directories outside the run directory: refused before anything is created.
    assert!(matches!(
        start(config(&other, &wrap)),
        Err(StartupError::TestBuild(
            GuardError::OutsideRunDirectory { .. }
        ))
    ));
    assert!(
        std::fs::read_dir(&other).unwrap().next().is_none(),
        "nothing written"
    );

    let services = start(config(&run, &wrap)).unwrap();
    assert_eq!(
        services.secret_backend(),
        SecretBackend::Vault,
        "never the keyring"
    );

    // Endpoints: loopback only, after resolution; the driver gets the checked literal.
    assert!(matches!(
        refused_by(open_at(&services, "10.0.0.1")),
        GuardError::NotLoopback { .. }
    ));
    assert!(matches!(
        refused_by(open_at(&services, "0.0.0.0")),
        GuardError::NotLoopback { .. }
    ));
    assert!(matches!(
        refused_by(open_at(&services, "db.invalid")),
        GuardError::Unresolved { .. }
    ));
    assert_eq!(wrap.seen.lock().unwrap().len(), 1, "no driver connect");
    match open_at(&services, "localhost").unwrap() {
        OpenOutcome::Opened(h) => drop(h),
        other => panic!("not opened: {other:?}"),
    }
    let seen = wrap.last_seen();
    match &seen.endpoint {
        ninesql_core::driver_api::Endpoint::Tcp { host, port } => {
            let ip: std::net::IpAddr = host.parse().expect("an IP literal");
            assert!(e2e::is_loopback(ip));
            assert_eq!(*port, 5000);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(seen.server_name.as_deref(), Some("localhost"));

    // SSH: every hop must be loopback too, refused before any connection or secret read.
    for (server, jump) in [("10.0.0.2", None), ("127.0.0.1", Some("192.0.2.7"))] {
        let mut c = mock_connection(ConnectionTypeId::dev());
        c.endpoint = Endpoint::Tcp {
            host: "127.0.0.1".into(),
            port: Some(5000),
            database: None,
        };
        c.tls.mode = TlsMode::Disable;
        let hop = |host: &str| SshHop {
            host: host.into(),
            port: 2222,
            username: "u".into(),
            auth: SshAuth::Agent,
            host_key_policy: HostKeyPolicy::Strict,
        };
        c.ssh = Some(SshTunnel {
            server: hop(server),
            jump_hosts: jump.map(hop).into_iter().collect(),
        });
        let e = refused_by(ui(open(
            services.sessions(),
            OpenRequest::new(c, &ConnectionTypes::default()),
        )));
        assert!(matches!(e, GuardError::NotLoopback { .. }), "{e:?}");
    }

    // Database files: inside the run directory only.
    let sqlite = |path: PathBuf| {
        let mut c = ninesql_core::Connection::new(
            "File",
            ninesql_core::EngineId::new("sqlite").unwrap(),
            Endpoint::File { path },
            ninesql_core::Auth::None,
            ConnectionTypeId::dev(),
        );
        c.tls.mode = TlsMode::Disable;
        ui(open(
            services.sessions(),
            OpenRequest::new(c, &ConnectionTypes::default()),
        ))
    };
    let outside_db = other.join("nine.sqlite");
    rusqlite::Connection::open(&outside_db).unwrap();
    assert!(matches!(
        refused_by(sqlite(outside_db)),
        GuardError::OutsideRunDirectory { .. }
    ));
    let inside_db = run.join("nine.sqlite");
    rusqlite::Connection::open(&inside_db).unwrap();
    match sqlite(inside_db).unwrap() {
        OpenOutcome::Opened(h) => drop(h),
        other => panic!("not opened: {other:?}"),
    }

    // Export targets and CSV import sources.
    let transfer = services.transfer();
    let refused = transfer
        .check_export_path(&other.join("rows.csv"), Overwrite::Refuse)
        .unwrap_err();
    assert!(
        matches!(refused, TransferError::ForbiddenLocation { .. }),
        "{refused:?}"
    );
    std::os::unix::fs::symlink(&other, run.join("away")).unwrap();
    assert!(
        transfer
            .check_export_path(&run.join("away").join("rows.csv"), Overwrite::Refuse)
            .is_err()
    );
    let out = run.join("out");
    std::fs::create_dir(&out).unwrap();
    transfer
        .check_export_path(&out.join("rows.csv"), Overwrite::Refuse)
        .unwrap();
    let csv = other.join("in.csv");
    std::fs::write(&csv, "id\n1\n").unwrap();
    let preview = ui(async {
        transfer
            .preview_csv(CsvPreviewRequest::new(
                &csv,
                vec![TargetColumn::new("id", "bigint")],
            ))
            .await
            .unwrap()
    });
    assert!(
        matches!(preview, Err(TransferError::ForbiddenLocation { .. })),
        "{preview:?}"
    );

    // "Export connections to DBeaver…": inside only (an empty choice then fails on its own).
    let exported = ui(async {
        services
            .export_dbeaver(Vec::new(), ExportTarget::File(other.join("ds.json")))
            .await
            .unwrap()
    });
    assert!(
        matches!(
            exported,
            Err(DbeaverExportError::TestBuild(
                GuardError::OutsideRunDirectory { .. }
            ))
        ),
        "{exported:?}"
    );
    let exported = ui(async {
        services
            .export_dbeaver(Vec::new(), ExportTarget::File(out.join("ds.json")))
            .await
            .unwrap()
    });
    assert!(
        matches!(exported, Err(DbeaverExportError::NothingSelected)),
        "{exported:?}"
    );

    // DBeaver import sources: inside only.
    let previewed = ui(async {
        services
            .preview_import(ImportSource::Folder(other.clone()))
            .await
            .unwrap()
    });
    assert!(
        matches!(
            previewed,
            Err(ImportError::TestBuild(
                GuardError::OutsideRunDirectory { .. }
            ))
        ),
        "{previewed:?}"
    );

    services.shutdown(Duration::from_secs(5));
}
