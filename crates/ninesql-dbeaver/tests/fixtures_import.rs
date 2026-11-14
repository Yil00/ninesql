//! Imports the five P0-E fixture workspaces (DBeaver 22.3.5 to 26.2.4) and checks that every
//! connection maps as tests/fixtures/README.md describes. Connections are looked up by name
//! (ids differ per version). Secret values are compared, never printed.

mod common;

use std::path::PathBuf;

use ninesql_dbeaver::mapping::{
    Auth, CredentialsStatus, Endpoint, ImportedConnection, ProjectImport, SecretSlot, SecretState,
    SshAuth, Status, TlsMode,
};
use ninesql_dbeaver::{InstallKind, IssueKind, Severity, import_project, open_folder};
use ninesql_driver_api::EngineId;
use ninesql_secrets::{ExposeSecret, SecretKind};

fn import(version: &str, ws: &std::path::Path) -> ProjectImport {
    let workspace = open_folder(ws).unwrap_or_else(|e| panic!("[{version}] open_folder: {e}"));
    assert_eq!(workspace.kind, InstallKind::Custom);
    assert_eq!(
        workspace
            .product_version
            .as_deref()
            .map(|v| v.starts_with(&format!("{version}."))),
        Some(true),
        "[{version}] product version"
    );
    assert_eq!(workspace.projects.len(), 1, "[{version}] projects");
    let project = &workspace.projects[0];
    assert_eq!(project.name, "General");
    assert_eq!(project.data_source_files.len(), 1);
    assert!(project.credentials_file.is_some());
    import_project(project)
}

fn conn<'a>(version: &str, p: &'a ProjectImport, name: &str) -> &'a ImportedConnection {
    p.connections
        .iter()
        .find(|c| c.name == name)
        .and_then(|c| c.imported())
        .unwrap_or_else(|| panic!("[{version}] connection {name} not imported"))
}

fn secret_is(c: &ImportedConnection, slot: SecretSlot, kind: SecretKind, expected: &str) -> bool {
    c.secret(slot)
        .is_some_and(|s| s.kind == kind && s.value.expose_secret() == expected)
}

fn has_issue(p: &ProjectImport, name: &str, kind: IssueKind, key_contains: Option<&str>) -> bool {
    p.connections
        .iter()
        .filter(|c| c.name == name)
        .flat_map(|c| &c.issues)
        .any(|i| {
            i.kind == kind
                && key_contains.is_none_or(|k| i.key.as_deref().is_some_and(|key| key.contains(k)))
        })
}

fn tcp(c: &ImportedConnection) -> (&str, Option<u16>, Option<&str>) {
    match &c.endpoint {
        Endpoint::Tcp {
            host,
            port,
            database,
        } => (host.as_str(), *port, database.as_deref()),
        Endpoint::File { .. } => panic!("{} is not a TCP endpoint", c.name),
    }
}

#[test]
fn every_fixture_maps_as_documented() {
    let fixtures = common::fixture_workspaces();
    assert!(fixtures.len() >= 5, "expected the five P0-E fixtures");
    for (v, ws) in fixtures {
        let v = v.as_str();
        let p = import(v, &ws);

        // Project level.
        assert_eq!(
            p.credentials,
            CredentialsStatus::Decrypted { passwords: 8 },
            "[{v}]"
        );
        assert_eq!(p.connections.len(), 10, "[{v}] connections");
        assert_eq!(p.secret_count(), 8, "[{v}] secrets to move");
        assert!(
            !p.issues.iter().any(|i| i.severity == Severity::Error),
            "[{v}] project errors: {:?}",
            p.issues
        );
        let folders: Vec<Vec<&str>> = p
            .folders
            .iter()
            .map(|f| f.path.iter().map(String::as_str).collect())
            .collect();
        assert_eq!(
            folders,
            [
                vec!["Analytics"],
                vec!["Legacy"],
                vec!["Team A"],
                vec!["Team A", "Production"],
                vec!["Team B"],
            ],
            "[{v}] folders"
        );
        assert_eq!(
            p.folders
                .iter()
                .find(|f| f.path == ["Team A"])
                .and_then(|f| f.description.as_deref()),
            Some("Synthetic fixture folder"),
            "[{v}]"
        );

        // Connection types.
        let ty = |id: &str| {
            p.connection_types
                .iter()
                .find(|t| t.dbeaver_id == id)
                .unwrap_or_else(|| panic!("[{v}] type {id}"))
        };
        let (dev, test, prod, staging) = (ty("dev"), ty("test"), ty("prod"), ty("staging"));
        assert!(dev.builtin && test.builtin && prod.builtin && !staging.builtin);
        assert!(prod.production && !prod.auto_commit && prod.confirm_destructive);
        assert!(!prod.log_query_text);
        assert!(test.auto_commit && test.confirm_destructive && !test.confirm_every_write);
        assert_eq!(dev.color.to_string(), "#FFFFFF");
        // Theme keys (22.x–25.x) and R,G,B (26.x) give the same colors.
        assert_eq!(prod.color.to_string(), "#FACFCF", "[{v}]");
        assert_eq!(test.color.to_string(), "#D6FACF", "[{v}]");
        assert_eq!(staging.id, "staging");
        assert_eq!(staging.name, "Staging");
        assert_eq!(staging.color.to_string(), "#008080");
        assert!(!staging.auto_commit && staging.confirm_every_write && staging.confirm_destructive);
        assert!(!staging.production && staging.log_query_text);

        // Every connection carries its DBeaver origin for idempotent re-import.
        for c in &p.connections {
            let imported = c
                .imported()
                .unwrap_or_else(|| panic!("[{v}] {} failed", c.name));
            assert_eq!(imported.origin.dbeaver_id, c.dbeaver_id);
            assert_eq!(imported.origin.project, "General");
            assert!(c.dbeaver_id.starts_with(&imported.dbeaver_driver));
        }

        // pg-dev
        let c = conn(v, &p, "pg-dev");
        assert_eq!(c.engine, EngineId::POSTGRESQL);
        assert_eq!(tcp(c), ("pg-dev.example.invalid", Some(5432), Some("nine")));
        assert_eq!(c.folder, ["Team A"]);
        assert_eq!(c.connection_type, "dev");
        assert_eq!(
            c.properties.keys().map(String::as_str).collect::<Vec<_>>(),
            ["ApplicationName", "connectTimeout"]
        );
        assert_eq!(c.init_sql, ["SET statement_timeout = 30000"]);
        assert_eq!(c.default_schema.as_deref(), Some("app"));
        assert!(has_issue(
            &p,
            "pg-dev",
            IssueKind::Unsupported,
            Some("defaultSchema")
        ));
        assert!(
            matches!(&c.auth, Auth::Password { username, secret: SecretState::Imported } if username == "nine_user")
        );
        assert!(secret_is(
            c,
            SecretSlot::Auth,
            SecretKind::Password,
            "fixture-not-a-secret"
        ));
        assert_eq!(c.tls.mode, TlsMode::VerifyFull);
        assert!(has_issue(&p, "pg-dev", IssueKind::TlsConverted, None));
        assert!(c.ssh.is_none() && c.enabled && c.read_only.is_none());

        // pg-prod-ssh
        let c = conn(v, &p, "pg-prod-ssh");
        assert_eq!(
            tcp(c),
            ("pg-prod.example.invalid", Some(5432), Some("nine_prod"))
        );
        assert_eq!(c.folder, ["Team A", "Production"]);
        assert_eq!(c.connection_type, "prod");
        assert_eq!(c.read_only, Some(true));
        assert!(
            matches!(&c.auth, Auth::Password { username, secret: SecretState::Imported } if username == "nine_ro")
        );
        assert!(secret_is(
            c,
            SecretSlot::Auth,
            SecretKind::Password,
            "fixture-prod-not-a-secret"
        ));
        let ssh = c
            .ssh
            .as_ref()
            .unwrap_or_else(|| panic!("[{v}] pg-prod-ssh has no SSH"));
        assert_eq!(ssh.server.host, "bastion.example.invalid");
        assert_eq!(ssh.server.port, 22);
        assert_eq!(ssh.server.username, "nine_ssh");
        assert_eq!(
            ssh.server.auth,
            SshAuth::Password {
                secret: SecretState::Imported
            }
        );
        assert!(secret_is(
            c,
            SecretSlot::SshServer,
            SecretKind::SshPassword,
            "fixture-ssh-not-a-secret"
        ));
        assert!(has_issue(
            &p,
            "pg-prod-ssh",
            IssueKind::SshHostKeyToConfirm,
            None
        ));
        assert_eq!(c.secrets.len(), 2);

        // pg-url-tls: JDBC URL only, TLS handler verify-full with certificates.
        let c = conn(v, &p, "pg-url-tls");
        assert_eq!(
            tcp(c),
            ("pg-url.example.invalid", Some(5433), Some("nine_url"))
        );
        assert_eq!(c.folder, ["Team B"]);
        assert!(
            c.properties.is_empty(),
            "[{v}] sslmode must not stay a property"
        );
        assert_eq!(c.tls.mode, TlsMode::VerifyFull);
        assert_eq!(
            c.tls.ca_file,
            Some(PathBuf::from("/home/nine_user/certs/fixture-ca.pem"))
        );
        let cert = c
            .tls
            .client_certificate
            .as_ref()
            .unwrap_or_else(|| panic!("[{v}] client cert"));
        assert_eq!(
            cert.cert_file,
            PathBuf::from("/home/nine_user/certs/fixture-client.pem")
        );
        assert_eq!(
            cert.key_file,
            PathBuf::from("/home/nine_user/certs/fixture-client.key")
        );
        assert!(
            matches!(&c.auth, Auth::Password { username, secret: SecretState::Prompt } if username == "nine_user")
        );
        assert!(c.secrets.is_empty());
        assert!(!has_issue(
            &p,
            "pg-url-tls",
            IssueKind::TlsNotVerified,
            None
        ));

        // timescale-metrics
        let c = conn(v, &p, "timescale-metrics");
        assert_eq!(c.engine, EngineId::TIMESCALEDB);
        assert_eq!(
            tcp(c),
            ("tsdb.example.invalid", Some(5432), Some("metrics"))
        );
        assert!(secret_is(
            c,
            SecretSlot::Auth,
            SecretKind::Password,
            "fixture-not-a-secret"
        ));

        // mysql-test: manual commit, driver properties, useSSL=true (opportunistic TLS).
        let c = conn(v, &p, "mysql-test");
        assert_eq!(c.engine, EngineId::MYSQL);
        assert_eq!(tcp(c), ("mysql.example.invalid", Some(3306), Some("nine")));
        assert_eq!(c.connection_type, "test");
        assert_eq!(c.auto_commit, Some(false));
        assert!(has_issue(
            &p,
            "mysql-test",
            IssueKind::Unsupported,
            Some("autocommit")
        ));
        assert_eq!(
            c.properties.keys().map(String::as_str).collect::<Vec<_>>(),
            ["connectTimeout"]
        );
        assert_eq!(c.tls.mode, TlsMode::Require);
        assert!(has_issue(&p, "mysql-test", IssueKind::TlsNotVerified, None));
        assert!(secret_is(
            c,
            SecretSlot::Auth,
            SecretKind::Password,
            "fixture-not-a-secret"
        ));

        // mariadb-ssh-key: public-key SSH with a saved passphrase, TLS without verification.
        let c = conn(v, &p, "mariadb-ssh-key");
        assert_eq!(c.engine, EngineId::MARIADB);
        assert!(c.folder.is_empty());
        let ssh = c
            .ssh
            .as_ref()
            .unwrap_or_else(|| panic!("[{v}] mariadb-ssh-key has no SSH"));
        assert_eq!(ssh.server.port, 2222);
        assert_eq!(ssh.server.username, "nine_ssh");
        assert_eq!(
            ssh.server.auth,
            SshAuth::PublicKey {
                key_path: PathBuf::from("/home/nine_user/.ssh/id_fixture_ed25519"),
                passphrase: Some(SecretState::Imported),
            }
        );
        assert!(secret_is(
            c,
            SecretSlot::SshServer,
            SecretKind::SshKeyPassphrase,
            "fixture-passphrase-not-a-secret"
        ));
        // An Ed25519 key by its name: not flagged as a possible RSA key file (ADR-0016 §1.2).
        assert!(!has_issue(
            &p,
            "mariadb-ssh-key",
            IssueKind::SshKeyType,
            None
        ));
        assert_eq!(
            c.tls.mode,
            TlsMode::Require,
            "[{v}] ssl.verify.server=false"
        );
        assert_eq!(
            c.tls.ca_file,
            Some(PathBuf::from("/home/nine_user/certs/fixture-ca.pem"))
        );
        assert!(has_issue(
            &p,
            "mariadb-ssh-key",
            IssueKind::TlsNotVerified,
            None
        ));
        assert!(
            matches!(&c.auth, Auth::Password { username, secret: SecretState::Prompt } if username == "nine_user")
        );
        assert_eq!(c.secrets.len(), 1);

        // sqlite-local
        let c = conn(v, &p, "sqlite-local");
        assert_eq!(c.engine, EngineId::SQLITE);
        assert_eq!(
            c.endpoint,
            Endpoint::File {
                path: PathBuf::from("/home/nine_user/data/fixture.sqlite")
            }
        );
        assert_eq!(c.auth, Auth::None);
        assert_eq!(
            c.description.as_deref(),
            Some("Local SQLite file (synthetic fixture)")
        );
        assert!(!has_issue(
            &p,
            "sqlite-local",
            IssueKind::TlsConverted,
            None
        ));

        // clickhouse-analytics: v0.2 engine (enabled), custom type.
        let c = conn(v, &p, "clickhouse-analytics");
        assert_eq!(c.engine, EngineId::CLICKHOUSE);
        assert!(c.enabled);
        assert_eq!(c.connection_type, "staging");
        assert_eq!(
            tcp(c),
            ("clickhouse.example.invalid", Some(8123), Some("default"))
        );
        assert!(secret_is(
            c,
            SecretSlot::Auth,
            SecretKind::Password,
            "fixture-not-a-secret"
        ));

        // oracle-legacy: outside the v1 set → disabled, raw ids kept, password still moved.
        let entry = p.connections.iter().find(|c| c.name == "oracle-legacy");
        assert_eq!(entry.map(|c| c.status()), Some(Status::ImportedDisabled));
        let c = conn(v, &p, "oracle-legacy");
        assert!(!c.enabled);
        assert!(c.disabled_reason.is_some());
        assert_eq!(c.engine.as_str(), "oracle");
        assert_eq!(
            (c.dbeaver_provider.as_str(), c.dbeaver_driver.as_str()),
            ("oracle", "oracle_thin")
        );
        assert!(has_issue(
            &p,
            "oracle-legacy",
            IssueKind::UnsupportedEngine,
            None
        ));
        assert!(secret_is(
            c,
            SecretSlot::Auth,
            SecretKind::Password,
            "fixture-not-a-secret"
        ));

        // roundtrip-marker
        let c = conn(v, &p, "roundtrip-marker");
        assert!(
            matches!(&c.auth, Auth::Password { username, secret: SecretState::Prompt } if username == "nine_user")
        );

        // Version-specific keys are classified, never reported as unknown.
        for c in &p.connections {
            for i in &c.issues {
                assert_ne!(
                    i.kind,
                    IssueKind::UnknownKey,
                    "[{v}] {}: unknown key {:?}",
                    c.name,
                    i.key
                );
            }
        }
        for i in &p.issues {
            assert_ne!(
                i.kind,
                IssueKind::UnknownKey,
                "[{v}] project: unknown key {:?}",
                i.key
            );
        }
    }
}

#[test]
fn reimport_is_stable() {
    // Same input → same ids, names and mapped values (idempotent re-import relies on dbeaver_id).
    for (v, ws) in common::fixture_workspaces() {
        let a = import(&v, &ws);
        let b = import(&v, &ws);
        let ids = |p: &ProjectImport| -> Vec<(String, String)> {
            p.connections
                .iter()
                .map(|c| (c.dbeaver_id.clone(), c.name.clone()))
                .collect()
        };
        assert_eq!(ids(&a), ids(&b), "[{v}]");
        assert_eq!(a.issues, b.issues, "[{v}]");
    }
}
