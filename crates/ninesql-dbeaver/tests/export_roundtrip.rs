//! P5-D: NineSQL connections → DBeaver export → NineSQL's importer gives equivalent connections,
//! with no secret, read-only and TLS never weakened; the export writes only new paths (I4).
// Test code: a failed unwrap is a failed test (AGENTS.md allows it outside library code).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use ninesql_dbeaver::export::{
    ExportAuth, ExportClientCertificate, ExportConnection, ExportConnectionType, ExportDisabled,
    ExportEndpoint, ExportHostKeyPolicy, ExportIssueKind, ExportOptions, ExportSshAuth,
    ExportSshHop, ExportSshTunnel, ExportTarget, ExportTls, ExportWriteError, Rgb, TlsMode,
};
use ninesql_dbeaver::mapping::{
    self, Auth, Endpoint, ImportedConnection, SecretState, SshAuth, TlsConfig,
};
use ninesql_dbeaver::{
    export_connections, import_project, open_folder, parse_data_sources, write_export,
};
use ninesql_driver_api::EngineId;
use proptest::prelude::*;

fn engine(id: &str) -> EngineId {
    EngineId::new(id.to_owned())
}

fn base(name: &str, engine_id: &str) -> ExportConnection {
    ExportConnection {
        id: format!("nine-{name}"),
        name: name.to_owned(),
        description: None,
        engine: engine(engine_id),
        endpoint: ExportEndpoint::Tcp {
            host: "db.example.test".to_owned(),
            port: Some(5432),
            database: Some("nine".to_owned()),
        },
        auth: ExportAuth::Password {
            username: "nine_user".to_owned(),
            saved: true,
        },
        scram_only: false,
        ssh: None,
        tls: ExportTls::default(),
        connection_type: "dev".to_owned(),
        read_only: None,
        folder: Vec::new(),
        properties: BTreeMap::new(),
        init_sql: Vec::new(),
        default_schema: None,
        default_catalog: None,
        auto_commit: None,
        disabled: None,
    }
}

fn staging() -> ExportConnectionType {
    ExportConnectionType {
        id: "staging".to_owned(),
        name: "Staging \"QA\" ü".to_owned(),
        color: Rgb {
            r: 0,
            g: 128,
            b: 128,
        },
        auto_commit: false,
        confirm_destructive: true,
        confirm_every_write: true,
        read_only: true,
        log_query_text: true,
        production: false,
    }
}

fn options() -> ExportOptions {
    let mut types: Vec<ExportConnectionType> = ExportConnectionType::builtins().into();
    types.push(staging());
    ExportOptions {
        connection_types: types,
        timestamp_millis: Some(0x01a1_dae6_7c7e),
    }
}

/// A representative set: every family, SSH with jump hosts, TLS modes, folders, properties.
fn sample(dir: &Path) -> Vec<ExportConnection> {
    let mut out = Vec::new();

    let mut pg = base("pg \"dev\" — ünïcode ✓", "postgresql");
    pg.description = Some("Synthetic 'export' test".to_owned());
    pg.folder = vec!["Team A".to_owned(), "Production".to_owned()];
    pg.properties = BTreeMap::from([
        ("search_path".to_owned(), "app,public".to_owned()),
        ("TimeZone".to_owned(), "Europe/Paris".to_owned()),
        ("statement_timeout".to_owned(), "30s".to_owned()),
        ("application_name".to_owned(), "dropped".to_owned()),
    ]);
    pg.init_sql = vec![
        "SET work_mem = '64MB'".to_owned(),
        "ALTER ROLE x PASSWORD 'made-up'".to_owned(),
    ];
    pg.default_schema = Some("app".to_owned());
    pg.default_catalog = Some("nine".to_owned());
    pg.tls = ExportTls {
        mode: TlsMode::VerifyFull,
        ca_file: Some(PathBuf::from("/home/nine_user/certs/ca.pem")),
        client_certificate: Some(ExportClientCertificate {
            cert_file: PathBuf::from("/home/nine_user/certs/client.pem"),
            key_file: PathBuf::from("/home/nine_user/certs/client.key"),
            has_passphrase: true,
        }),
    };
    out.push(pg);

    let mut prod = base("pg-prod-ssh", "postgresql");
    prod.connection_type = "prod".to_owned();
    prod.read_only = Some(true);
    prod.auto_commit = Some(true);
    prod.folder = vec!["Team A".to_owned()];
    prod.tls.mode = TlsMode::Require;
    prod.ssh = Some(ExportSshTunnel {
        server: ExportSshHop {
            host: "bastion.example.test".to_owned(),
            port: 2222,
            username: "nine_ssh".to_owned(),
            auth: ExportSshAuth::Password { saved: true },
            host_key_policy: ExportHostKeyPolicy::Strict,
        },
        jump_hosts: vec![
            ExportSshHop {
                host: "jump1.example.test".to_owned(),
                port: 22,
                username: "jumper".to_owned(),
                auth: ExportSshAuth::Agent,
                host_key_policy: ExportHostKeyPolicy::AcceptNew,
            },
            ExportSshHop {
                host: "jump2.example.test".to_owned(),
                port: 2200,
                username: "jumper2".to_owned(),
                auth: ExportSshAuth::PublicKey {
                    key_path: PathBuf::from("/home/nine_user/.ssh/id_ed25519"),
                    passphrase: Some(false),
                },
                host_key_policy: ExportHostKeyPolicy::Strict,
            },
        ],
    });
    out.push(prod);

    let mut ts = base("timescale", "timescaledb");
    ts.tls.mode = TlsMode::Disable;
    ts.connection_type = "staging".to_owned();
    out.push(ts);

    let mut my = base("mysql-test", "mysql");
    my.endpoint = ExportEndpoint::Tcp {
        host: "mysql.example.test".to_owned(),
        port: Some(3306),
        database: Some("nine's db".to_owned()),
    };
    my.connection_type = "test".to_owned();
    my.auto_commit = Some(false);
    my.properties = BTreeMap::from([
        ("time_zone".to_owned(), "+00:00".to_owned()),
        ("lock_wait_timeout".to_owned(), "10".to_owned()),
        ("max_execution_time".to_owned(), "5000".to_owned()),
    ]);
    my.tls = ExportTls {
        mode: TlsMode::VerifyCa,
        ca_file: Some(PathBuf::from("/home/nine_user/certs/ca.pem")),
        client_certificate: None,
    };
    out.push(my);

    let mut maria = base("mariadb-key", "mariadb");
    maria.endpoint = ExportEndpoint::Tcp {
        host: "mariadb.example.test".to_owned(),
        port: None,
        database: None,
    };
    maria.auth = ExportAuth::Username {
        username: "nine_ro".to_owned(),
    };
    maria.properties = BTreeMap::from([("max_statement_time".to_owned(), "2.5".to_owned())]);
    maria.tls = ExportTls {
        mode: TlsMode::VerifyFull,
        ca_file: Some(PathBuf::from("/home/nine_user/certs/ca.pem")),
        client_certificate: None,
    };
    maria.ssh = Some(ExportSshTunnel {
        server: ExportSshHop {
            host: "bastion.example.test".to_owned(),
            port: 22,
            username: "nine_ssh".to_owned(),
            auth: ExportSshAuth::PublicKey {
                key_path: PathBuf::from("/home/nine_user/.ssh/id_fixture_ed25519"),
                passphrase: None,
            },
            host_key_policy: ExportHostKeyPolicy::Strict,
        },
        jump_hosts: Vec::new(),
    });
    out.push(maria);

    let mut tidb = base("tidb", "tidb");
    tidb.tls.mode = TlsMode::Disable;
    out.push(tidb);

    let mut sqlite = base("sqlite-local", "sqlite");
    sqlite.endpoint = ExportEndpoint::File {
        path: dir.join("local.sqlite"),
    };
    sqlite.auth = ExportAuth::None;
    sqlite.properties = BTreeMap::from([
        ("busy_timeout".to_owned(), "5000".to_owned()),
        ("foreign_keys".to_owned(), "true".to_owned()),
    ]);
    out.push(sqlite);

    let mut ch = base("clickhouse", "clickhouse");
    ch.properties = BTreeMap::from([
        ("socket_timeout".to_owned(), "30000".to_owned()),
        ("password".to_owned(), "made-up-not-a-secret".to_owned()),
    ]);
    out.push(ch);

    let mut quest = base("questdb", "questdb");
    quest.tls.mode = TlsMode::VerifyFull;
    out.push(quest);

    let mut route = base("behind-proxy", "postgresql");
    route.disabled = Some(ExportDisabled::Route);
    out.push(route);

    let mut socket = base("socket", "postgresql");
    socket.endpoint = ExportEndpoint::UnixSocket {
        path: PathBuf::from("/run/postgresql"),
        database: None,
    };
    out.push(socket);

    let mut oracle = base("oracle", "oracle");
    oracle.disabled = Some(ExportDisabled::NoDriver);
    out.push(oracle);
    out
}

/// Exports to a new project folder under `dir` and imports it back with the real importer.
fn round_trip(
    conns: &[ExportConnection],
    dir: &Path,
) -> (
    ninesql_dbeaver::Export,
    ninesql_dbeaver::ProjectImport,
    PathBuf,
) {
    let export = export_connections(conns, &options()).expect("export");
    let folder = dir.join("Exported");
    let written = write_export(
        &export,
        &ExportTarget::ProjectFolder(folder.clone()),
        &|_| false,
    )
    .expect("write");
    assert_eq!(
        written.data_sources,
        folder.join(".dbeaver/data-sources.json")
    );
    let ws = open_folder(&folder).expect("open exported folder");
    assert_eq!(ws.projects.len(), 1);
    let imported = import_project(&ws.projects[0]);
    (export, imported, folder)
}

fn by_name<'a>(imp: &'a ninesql_dbeaver::ProjectImport, name: &str) -> &'a ImportedConnection {
    imp.connections
        .iter()
        .find(|c| c.name == name)
        .and_then(|c| c.imported())
        .unwrap_or_else(|| panic!("connection {name} not imported"))
}

#[test]
fn export_imports_back_equivalent() {
    let dir = tempfile::tempdir().unwrap();
    let conns = sample(dir.path());
    let (export, imp, folder) = round_trip(&conns, dir.path());

    // Skipped: route-disabled, Unix socket, engine without a DBeaver driver.
    let skipped: Vec<&str> = export
        .report
        .connections
        .iter()
        .filter(|c| !c.exported())
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(skipped, ["behind-proxy", "socket", "oracle"]);
    assert_eq!(imp.connections.len(), conns.len() - 3);
    assert_eq!(imp.secret_count(), 0, "no secret may come back");
    assert!(!folder.join(".dbeaver/credentials-config.json").exists());

    // PostgreSQL: names with quotes and unicode, folder, properties, TLS paths, bootstrap.
    let pg = by_name(&imp, "pg \"dev\" — ünïcode ✓");
    assert_eq!(pg.engine, engine("postgresql"));
    assert_eq!(pg.description.as_deref(), Some("Synthetic 'export' test"));
    assert_eq!(pg.folder, ["Team A", "Production"]);
    assert_eq!(
        pg.endpoint,
        Endpoint::Tcp {
            host: "db.example.test".to_owned(),
            port: Some(5432),
            database: Some("nine".to_owned())
        }
    );
    assert_eq!(
        pg.auth,
        Auth::Password {
            username: "nine_user".to_owned(),
            secret: SecretState::Prompt
        }
    );
    assert_eq!(
        pg.properties,
        BTreeMap::from([
            ("search_path".to_owned(), "app,public".to_owned()),
            ("TimeZone".to_owned(), "Europe/Paris".to_owned()),
            ("statement_timeout".to_owned(), "30s".to_owned()),
        ])
    );
    assert_eq!(
        pg.init_sql,
        ["SET work_mem = '64MB'"],
        "credential statement dropped"
    );
    assert_eq!(pg.default_schema.as_deref(), Some("app"));
    assert_eq!(pg.default_catalog.as_deref(), Some("nine"));
    assert_eq!(pg.tls.mode, mapping::TlsMode::VerifyFull);
    assert_eq!(
        pg.tls.ca_file.as_deref(),
        Some(Path::new("/home/nine_user/certs/ca.pem"))
    );
    let cert = pg.tls.client_certificate.as_ref().unwrap();
    assert_eq!(
        cert.cert_file,
        Path::new("/home/nine_user/certs/client.pem")
    );
    assert!(pg.enabled);
    let pg_report = &export.report.connections[0];
    for kind in [
        ExportIssueKind::SecretOmitted,
        ExportIssueKind::LooksLikeSecret,
    ] {
        assert!(pg_report.issues.iter().any(|i| i.kind == kind), "{kind:?}");
    }
    assert!(
        pg_report
            .issues
            .iter()
            .any(|i| i.field.as_deref() == Some("properties.application_name"))
    );

    // Production with SSH and jump hosts: read-only kept, auto-commit never turned on.
    let prod = by_name(&imp, "pg-prod-ssh");
    assert_eq!(prod.connection_type, "prod");
    assert_eq!(prod.read_only, Some(true));
    assert_eq!(prod.auto_commit, None);
    assert_eq!(prod.tls.mode, mapping::TlsMode::Require);
    let ssh = prod.ssh.as_ref().unwrap();
    assert_eq!(ssh.server.host, "bastion.example.test");
    assert_eq!(ssh.server.port, 2222);
    assert_eq!(ssh.server.username, "nine_ssh");
    assert_eq!(
        ssh.server.auth,
        SshAuth::Password {
            secret: SecretState::Prompt
        }
    );
    assert_eq!(ssh.jump_hosts.len(), 2);
    assert_eq!(ssh.jump_hosts[0].host, "jump1.example.test");
    assert_eq!(ssh.jump_hosts[0].auth, SshAuth::Agent);
    assert_eq!(ssh.jump_hosts[1].port, 2200);
    assert_eq!(ssh.jump_hosts[1].username, "jumper2");
    assert!(matches!(
        &ssh.jump_hosts[1].auth,
        SshAuth::PublicKey { key_path, passphrase: Some(SecretState::Prompt) }
            if key_path == Path::new("/home/nine_user/.ssh/id_ed25519")
    ));

    // Custom type: its read-only default becomes the connection flag; TLS off stays explicit.
    let ts = by_name(&imp, "timescale");
    assert_eq!(ts.engine, engine("timescaledb"));
    assert_eq!(ts.connection_type, "staging");
    assert_eq!(ts.read_only, Some(true));
    assert_eq!(ts.tls.mode, mapping::TlsMode::Disable);
    let staging = imp
        .connection_types
        .iter()
        .find(|t| t.id == "staging")
        .unwrap();
    assert_eq!(staging.name, "Staging \"QA\" ü");
    assert_eq!(
        staging.color,
        Rgb {
            r: 0,
            g: 128,
            b: 128
        }
    );
    assert!(!staging.auto_commit && staging.confirm_destructive && staging.confirm_every_write);

    // MySQL: session variables, verify-ca with its CA, manual commit, database with a quote.
    let my = by_name(&imp, "mysql-test");
    assert_eq!(my.connection_type, "test");
    assert_eq!(my.auto_commit, Some(false));
    assert_eq!(
        my.properties,
        BTreeMap::from([
            ("time_zone".to_owned(), "+00:00".to_owned()),
            ("lock_wait_timeout".to_owned(), "10".to_owned()),
            ("max_execution_time".to_owned(), "5000".to_owned()),
        ])
    );
    assert_eq!(my.tls.mode, mapping::TlsMode::VerifyCa);
    assert!(my.tls.ca_file.is_some());
    assert!(matches!(&my.endpoint, Endpoint::Tcp { database: Some(d), .. } if d == "nine's db"));

    // MariaDB: verify-full kept (not the handler, which would be verify-ca), CA kept.
    let maria = by_name(&imp, "mariadb-key");
    assert_eq!(maria.engine, engine("mariadb"));
    assert_eq!(maria.tls.mode, mapping::TlsMode::VerifyFull);
    assert_eq!(
        maria.tls.ca_file.as_deref(),
        Some(Path::new("/home/nine_user/certs/ca.pem"))
    );
    assert_eq!(
        maria.properties,
        BTreeMap::from([("max_statement_time".to_owned(), "2.5".to_owned())])
    );
    assert!(matches!(
        &maria.endpoint,
        Endpoint::Tcp {
            port: None,
            database: None,
            ..
        }
    ));
    let key = &maria.ssh.as_ref().unwrap().server.auth;
    assert!(
        matches!(
            key,
            SshAuth::PublicKey {
                passphrase: None,
                ..
            }
        ),
        "{key:?}"
    );

    let tidb = by_name(&imp, "tidb");
    assert_eq!(tidb.engine, engine("tidb"));
    assert_eq!(tidb.tls.mode, mapping::TlsMode::Disable);

    // SQLite: file path, properties.
    let sqlite = by_name(&imp, "sqlite-local");
    assert_eq!(sqlite.auth, Auth::None);
    assert!(matches!(&sqlite.endpoint, Endpoint::File { path } if path.ends_with("local.sqlite")));
    assert_eq!(sqlite.properties.len(), 2);

    // ClickHouse (no NineSQL driver yet): properties kept as is, secret-like ones dropped.
    let ch = by_name(&imp, "clickhouse");
    assert_eq!(
        ch.properties,
        BTreeMap::from([("socket_timeout".to_owned(), "30000".to_owned())])
    );
    assert_eq!(
        ch.tls,
        TlsConfig::default(),
        "never weaker than verify-full"
    );

    // QuestDB: approximated as PostgreSQL, reported.
    assert_eq!(by_name(&imp, "questdb").engine, engine("postgresql"));
}

#[test]
fn exported_bytes_hold_no_secret_and_parse_as_dbeaver_data_sources() {
    let dir = tempfile::tempdir().unwrap();
    let export = export_connections(&sample(dir.path()), &options()).unwrap();
    let text = String::from_utf8(export.json.clone()).unwrap();
    for needle in [common::SECRET_MARKER, "made-up", "PASSWORD '"] {
        assert!(!text.contains(needle), "{needle}");
    }
    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert!(ninesql_storage::hygiene::find_secret_like_keys(&value).is_empty());
    let parsed = parse_data_sources(&export.json).unwrap();
    assert_eq!(parsed.connections.len(), export.report.exported_count());
    // DBeaver-style ids: `<driver>-<hex>-<hex>`.
    for c in &parsed.connections {
        let driver = c.driver.as_deref().unwrap();
        let rest =
            c.id.strip_prefix(driver)
                .unwrap()
                .strip_prefix('-')
                .unwrap();
        let (a, b) = rest.split_once('-').unwrap();
        assert!(u64::from_str_radix(a, 16).is_ok() && u64::from_str_radix(b, 16).is_ok());
    }
    // `Debug` of the export shows no content.
    assert!(!format!("{export:?}").contains("db.example.test"));
}

#[test]
fn single_file_target_is_new_and_private() {
    let dir = tempfile::tempdir().unwrap();
    let export = export_connections(&sample(dir.path()), &options()).unwrap();
    let target = dir.path().join("data-sources-ninesql.json");
    let written = write_export(&export, &ExportTarget::File(target.clone()), &|_| false).unwrap();
    assert_eq!(written.data_sources, fs::canonicalize(&target).unwrap());
    assert_eq!(fs::read(&target).unwrap(), export.json);
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        fs::metadata(&target).unwrap().permissions().mode() & 0o777,
        0o600
    );
    // No temporary file left behind.
    let names: Vec<String> = fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["data-sources-ninesql.json"]);

    // Never overwritten.
    let err = write_export(&export, &ExportTarget::File(target.clone()), &|_| false).unwrap_err();
    assert!(
        matches!(err, ExportWriteError::AlreadyExists { .. }),
        "{err}"
    );
    assert_eq!(fs::read(&target).unwrap(), export.json);
}

#[test]
fn unsafe_targets_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let export = export_connections(&sample(dir.path()), &options()).unwrap();
    let refuse = |target: ExportTarget| write_export(&export, &target, &|_| false).unwrap_err();

    // An existing file is never replaced, even an unrelated one.
    let existing = dir.path().join("existing.json");
    fs::write(&existing, b"keep me").unwrap();
    assert!(matches!(
        refuse(ExportTarget::File(existing.clone())),
        ExportWriteError::AlreadyExists { .. }
    ));
    assert_eq!(fs::read(&existing).unwrap(), b"keep me");

    // A dangling symbolic link at the target is not followed.
    let link = dir.path().join("link.json");
    std::os::unix::fs::symlink(dir.path().join("elsewhere.json"), &link).unwrap();
    assert!(matches!(
        refuse(ExportTarget::File(link)),
        ExportWriteError::AlreadyExists { .. }
    ));
    assert!(!dir.path().join("elsewhere.json").exists());

    // An existing folder is not reused as a project.
    let folder = dir.path().join("folder");
    fs::create_dir(&folder).unwrap();
    assert!(matches!(
        refuse(ExportTarget::ProjectFolder(folder.clone())),
        ExportWriteError::AlreadyExists { .. }
    ));
    assert_eq!(fs::read_dir(&folder).unwrap().count(), 0);

    // Relative, missing directory, reserved name.
    assert!(matches!(
        refuse(ExportTarget::File(PathBuf::from("out.json"))),
        ExportWriteError::NotAbsolute { .. }
    ));
    assert!(matches!(
        refuse(ExportTarget::File(dir.path().join("missing/out.json"))),
        ExportWriteError::NoDirectory { .. }
    ));
    assert!(matches!(
        refuse(ExportTarget::File(
            dir.path().join("credentials-config.json")
        )),
        ExportWriteError::InvalidName { .. }
    ));

    // Inside a `DBeaverData` directory.
    let data = dir.path().join("DBeaverData");
    fs::create_dir(&data).unwrap();
    assert!(matches!(
        refuse(ExportTarget::File(data.join("out.json"))),
        ExportWriteError::InsideDbeaver { .. }
    ));

    // The caller's check (`DbeaverRoots` in the core).
    let protected = dir.path().join("picked");
    fs::create_dir(&protected).unwrap();
    let canon = fs::canonicalize(&protected).unwrap();
    let err = write_export(
        &export,
        &ExportTarget::File(protected.join("out.json")),
        &|p: &Path| p.starts_with(&canon),
    )
    .unwrap_err();
    assert!(
        matches!(err, ExportWriteError::InsideDbeaver { .. }),
        "{err}"
    );
    assert_eq!(fs::read_dir(&protected).unwrap().count(), 0);
}

#[test]
fn targets_inside_a_dbeaver_workspace_are_refused_and_its_files_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let export = export_connections(&sample(dir.path()), &options()).unwrap();
    for (version, ws) in common::fixture_workspaces() {
        // Copied under a neutral name, so only the workspace markers can refuse it.
        let copy = dir.path().join(format!("ws-{version}"));
        common::copy_tree(&ws, &copy).unwrap();
        let before = snapshot(&copy);
        let targets = [
            ExportTarget::File(copy.join("out.json")),
            ExportTarget::File(copy.join("General/out.json")),
            ExportTarget::File(copy.join("General/.dbeaver/data-sources-2.json")),
            ExportTarget::ProjectFolder(copy.join("NewProject")),
            ExportTarget::ProjectFolder(copy.join("General/sub")),
        ];
        for target in targets {
            let err = write_export(&export, &target, &|_| false).unwrap_err();
            assert!(
                matches!(err, ExportWriteError::InsideDbeaver { .. }),
                "{version}: {target:?}: {err}"
            );
        }
        assert_eq!(snapshot(&copy), before, "{version}: workspace changed");
    }
}

/// Every file under `root` with its bytes, sorted.
fn snapshot(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push((p.clone(), fs::read(&p).unwrap()));
            }
        }
    }
    out.sort();
    out
}

#[test]
fn reimporting_a_fixture_export_keeps_the_mapped_fields() {
    // DBeaver fixture → NineSQL import → export → import: the mapped fields survive.
    for (version, ws) in common::fixture_workspaces() {
        let ws = open_folder(&ws).unwrap();
        let first = import_project(&ws.projects[0]);
        let conns: Vec<ExportConnection> = first
            .connections
            .iter()
            .filter_map(|c| c.imported())
            .map(from_imported)
            .collect();
        let mut opts = options();
        opts.connection_types = first
            .connection_types
            .iter()
            .map(|t| ExportConnectionType {
                id: t.id.clone(),
                name: t.name.clone(),
                color: t.color,
                auto_commit: t.auto_commit,
                confirm_destructive: t.confirm_destructive,
                confirm_every_write: t.confirm_every_write,
                read_only: t.read_only,
                log_query_text: t.log_query_text,
                production: t.production,
            })
            .collect();
        let dir = tempfile::tempdir().unwrap();
        let export = export_connections(&conns, &opts).unwrap();
        let folder = dir.path().join("p");
        write_export(
            &export,
            &ExportTarget::ProjectFolder(folder.clone()),
            &|_| false,
        )
        .unwrap();
        // I1: the fixture's saved passwords (decrypted by the first import) appear nowhere in
        // what the export wrote.
        assert!(
            first.secret_count() > 0,
            "{version}: the fixture has saved secrets"
        );
        for (path, bytes) in snapshot(&folder) {
            let text = String::from_utf8_lossy(&bytes);
            assert!(
                !text.contains(common::SECRET_MARKER),
                "{version}: {} holds a secret",
                path.display()
            );
        }
        let second = import_project(&open_folder(&folder).unwrap().projects[0]);
        for c in first.connections.iter().filter_map(|c| c.imported()) {
            if c.engine == engine("oracle") {
                continue; // No DBeaver driver known on the way back: skipped, reported.
            }
            let back = by_name(&second, &c.name);
            let ctx = format!("{version}: {}", c.name);
            assert_eq!(back.engine, c.engine, "{ctx}");
            assert_eq!(back.endpoint, c.endpoint, "{ctx}");
            assert_eq!(back.folder, c.folder, "{ctx}");
            assert_eq!(back.connection_type, c.connection_type, "{ctx}");
            assert_eq!(back.read_only, c.read_only, "{ctx}");
            assert_eq!(back.init_sql, c.init_sql, "{ctx}");
            assert_eq!(back.default_schema, c.default_schema, "{ctx}");
            assert_eq!(back.auto_commit, c.auto_commit, "{ctx}");
            assert!(back.tls.mode >= c.tls.mode, "{ctx}: TLS weakened");
            assert_eq!(back.ssh.is_some(), c.ssh.is_some(), "{ctx}");
            assert_eq!(back.auth.username(), c.auth.username(), "{ctx}");
            assert!(back.secrets.is_empty(), "{ctx}");
        }
    }
}

trait Username {
    fn username(&self) -> Option<&str>;
}

impl Username for Auth {
    fn username(&self) -> Option<&str> {
        match self {
            Auth::None => None,
            Auth::Password { username, .. } => {
                Some(username).filter(|u| !u.is_empty()).map(|u| u.as_str())
            }
        }
    }
}

/// What the core would build from an imported connection (secrets saved).
fn from_imported(c: &ImportedConnection) -> ExportConnection {
    let hop = |h: &mapping::SshHop| ExportSshHop {
        host: h.host.clone(),
        port: h.port,
        username: h.username.clone(),
        auth: match &h.auth {
            SshAuth::Agent => ExportSshAuth::Agent,
            SshAuth::Password { secret } => ExportSshAuth::Password {
                saved: *secret == SecretState::Imported,
            },
            SshAuth::PublicKey {
                key_path,
                passphrase,
            } => ExportSshAuth::PublicKey {
                key_path: key_path.clone(),
                passphrase: passphrase.map(|s| s == SecretState::Imported),
            },
        },
        host_key_policy: ExportHostKeyPolicy::Strict,
    };
    ExportConnection {
        id: c.origin.dbeaver_id.clone(),
        name: c.name.clone(),
        description: c.description.clone(),
        engine: c.engine.clone(),
        endpoint: match &c.endpoint {
            Endpoint::Tcp {
                host,
                port,
                database,
            } => ExportEndpoint::Tcp {
                host: host.clone(),
                port: *port,
                database: database.clone(),
            },
            Endpoint::File { path } => ExportEndpoint::File { path: path.clone() },
        },
        auth: match &c.auth {
            Auth::None => ExportAuth::None,
            Auth::Password { username, secret } => ExportAuth::Password {
                username: username.clone(),
                saved: *secret == SecretState::Imported,
            },
        },
        scram_only: false,
        ssh: c.ssh.as_ref().map(|s| ExportSshTunnel {
            server: hop(&s.server),
            jump_hosts: s.jump_hosts.iter().map(hop).collect(),
        }),
        tls: ExportTls {
            mode: c.tls.mode,
            ca_file: c.tls.ca_file.clone(),
            client_certificate: c.tls.client_certificate.as_ref().map(|cc| {
                ExportClientCertificate {
                    cert_file: cc.cert_file.clone(),
                    key_file: cc.key_file.clone(),
                    has_passphrase: cc.passphrase.is_some(),
                }
            }),
        },
        connection_type: c.connection_type.clone(),
        read_only: c.read_only,
        folder: c.folder.clone(),
        properties: c.properties.clone(),
        init_sql: c.init_sql.clone(),
        default_schema: c.default_schema.clone(),
        default_catalog: c.default_catalog.clone(),
        auto_commit: c.auto_commit,
        disabled: (!c.enabled).then_some(ExportDisabled::NoDriver),
    }
}

// ---------------------------------------------------------------------------------------------
// Property tests: names with quotes and unicode, random settings; pure (no files).
// ---------------------------------------------------------------------------------------------

fn text() -> impl Strategy<Value = String> {
    // Printable text with quotes, backslashes, `=`, `,`, unicode; no control characters.
    proptest::string::string_regex("[a-zA-Z0-9 '\"\\\\=,;:/.ü✓日本語—-]{1,40}")
        .unwrap()
        .prop_map(|s| s.trim().to_owned())
        .prop_filter("not blank", |s| !s.is_empty())
}

fn safe_text() -> impl Strategy<Value = String> {
    // Free text that never trips the credential detector.
    text().prop_filter("no credential word", |s| {
        !ninesql_driver_api::hygiene::text_may_hold_credentials(s)
    })
}

fn tls_mode() -> impl Strategy<Value = TlsMode> {
    prop_oneof![
        Just(TlsMode::Disable),
        Just(TlsMode::Require),
        Just(TlsMode::VerifyCa),
        Just(TlsMode::VerifyFull),
    ]
}

fn connection() -> impl Strategy<Value = ExportConnection> {
    (
        safe_text(),
        prop_oneof![
            Just("postgresql"),
            Just("timescaledb"),
            Just("mysql"),
            Just("mariadb"),
            Just("tidb"),
            Just("sqlite"),
        ],
        proptest::option::of(1u16..),
        proptest::option::of(safe_text()),
        proptest::option::of(safe_text()),
        tls_mode(),
        proptest::option::of(any::<bool>()),
        prop_oneof![Just("dev"), Just("test"), Just("prod"), Just("staging")],
        proptest::collection::vec(safe_text().prop_map(|s| s.replace('/', "")), 0..3),
        proptest::option::of(any::<bool>()),
        proptest::option::of(safe_text()),
        any::<bool>(),
    )
        .prop_map(
            |(name, eng, port, database, user, mode, ro, ty, folder, auto, schema, ssh)| {
                let mut c = base(&name, eng);
                if eng == "sqlite" {
                    c.endpoint = ExportEndpoint::File {
                        path: PathBuf::from(format!("/srv/data/{}.sqlite", name.len())),
                    };
                    c.auth = ExportAuth::None;
                } else {
                    c.endpoint = ExportEndpoint::Tcp {
                        host: "db.example.test".to_owned(),
                        port,
                        database: database
                            .map(|d| d.trim().to_owned())
                            .filter(|d| !d.is_empty()),
                    };
                    c.auth = match user {
                        Some(u) => ExportAuth::Password {
                            username: u.trim().to_owned(),
                            saved: true,
                        },
                        None => ExportAuth::None,
                    };
                    c.tls.mode = mode;
                    if ssh {
                        c.ssh = Some(ExportSshTunnel {
                            server: ExportSshHop {
                                host: "bastion.example.test".to_owned(),
                                port: 22,
                                username: format!("u_{}", name.replace(' ', "_")),
                                auth: ExportSshAuth::Password { saved: false },
                                host_key_policy: ExportHostKeyPolicy::Strict,
                            },
                            jump_hosts: Vec::new(),
                        });
                    }
                }
                c.read_only = ro;
                c.connection_type = ty.to_owned();
                c.folder = folder
                    .into_iter()
                    .map(|f| f.trim().to_owned())
                    .filter(|f| !f.is_empty())
                    .collect();
                c.auto_commit = auto;
                c.default_schema = schema
                    .map(|s| s.trim().to_owned())
                    .filter(|s| !s.is_empty());
                c.description = Some(name.clone());
                c
            },
        )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    #[test]
    fn round_trip_is_equivalent_and_never_weaker(conns in proptest::collection::vec(connection(), 1..6)) {
        let opts = options();
        let export = export_connections(&conns, &opts).unwrap();
        let parsed = parse_data_sources(&export.json).unwrap();
        let imp = mapping::map_sources("General", &[parsed], None);
        prop_assert_eq!(imp.secret_count(), 0);
        prop_assert_eq!(export.report.exported_count(), conns.len());
        for (c, rep) in conns.iter().zip(&export.report.connections) {
            let id = rep.dbeaver_id.as_deref().unwrap();
            let back = imp
                .connections
                .iter()
                .find(|x| x.dbeaver_id == id)
                .and_then(|x| x.imported())
                .unwrap();
            prop_assert_eq!(&back.name, &c.name);
            prop_assert_eq!(&back.description, &c.description);
            prop_assert_eq!(&back.engine, &c.engine);
            prop_assert_eq!(&back.folder, &c.folder);
            prop_assert_eq!(&back.connection_type, &c.connection_type);
            prop_assert_eq!(&back.default_schema, &c.default_schema);
            let ty = opts.connection_types.iter().find(|t| t.id == c.connection_type).unwrap();
            let effective_ro = c.read_only.unwrap_or(ty.read_only);
            prop_assert_eq!(back.read_only, effective_ro.then_some(true));
            let expected_auto = match c.auto_commit {
                Some(true) if ty.is_production() => None,
                other => other,
            };
            prop_assert_eq!(back.auto_commit, expected_auto);
            match (&c.endpoint, &back.endpoint) {
                (ExportEndpoint::Tcp { host, port, database }, Endpoint::Tcp { host: h, port: p, database: d }) => {
                    prop_assert_eq!(host, h);
                    prop_assert_eq!(port, p);
                    prop_assert_eq!(database, d);
                    // Same TLS mode in every family the export writes TLS for.
                    prop_assert_eq!(back.tls.mode, c.tls.mode);
                }
                (ExportEndpoint::File { path }, Endpoint::File { path: p }) => prop_assert_eq!(path, p),
                _ => prop_assert!(false, "endpoint kind changed"),
            }
            let expected_auth = match &c.auth {
                ExportAuth::Password { username, .. } if !username.is_empty() => Auth::Password {
                    username: username.clone(),
                    secret: SecretState::Prompt,
                },
                _ => Auth::None,
            };
            prop_assert_eq!(&back.auth, &expected_auth);
            prop_assert_eq!(back.ssh.as_ref().map(|s| &s.server.username), c.ssh.as_ref().map(|s| &s.server.username));
            prop_assert!(back.enabled);
        }
    }
}

fn any_text() -> impl Strategy<Value = String> {
    // Anything, control characters and lone surrogate-free unicode included.
    proptest::string::string_regex("(?s).{0,24}").unwrap()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Arbitrary text in every field: the export never panics, its output always parses as a
    /// DBeaver file, holds no secret-like key, and the importer maps it without panicking.
    #[test]
    fn export_is_total_on_arbitrary_text(
        name in any_text(),
        host in any_text(),
        user in any_text(),
        folder in proptest::collection::vec(any_text(), 0..3),
        props in proptest::collection::btree_map(any_text(), any_text(), 0..4),
        init in proptest::collection::vec(any_text(), 0..3),
        ty in prop_oneof![Just("dev".to_owned()), Just("staging".to_owned()), any_text()],
        engine_id in prop_oneof![Just("postgresql"), Just("mysql"), Just("mariadb"), Just("clickhouse"), Just("questdb")],
    ) {
        let mut c = base(&name, engine_id);
        c.endpoint = ExportEndpoint::Tcp { host, port: None, database: Some(name.clone()) };
        c.auth = ExportAuth::Password { username: user, saved: true };
        c.folder = folder;
        c.properties = props;
        c.init_sql = init;
        c.connection_type = ty;
        c.description = Some(name);
        let export = export_connections(&[c], &options()).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&export.json).unwrap();
        prop_assert!(ninesql_storage::hygiene::find_secret_like_keys(&value).is_empty());
        let parsed = parse_data_sources(&export.json).unwrap();
        let imp = mapping::map_sources("General", &[parsed], None);
        prop_assert_eq!(imp.secret_count(), 0);
    }
}
