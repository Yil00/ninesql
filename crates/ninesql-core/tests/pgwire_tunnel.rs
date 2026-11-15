//! Core + pgwire integration (wave 2 integration review L5): `CoreServices` with the real
//! PostgreSQL driver, the database password in the vault, a **strict** SSH tunnel (the unknown
//! host key is confirmed first), TLS **verify-full** through the tunnel against the dev
//! service's throwaway CA (`server_name` = the real database host), a **read-only** session, and
//! a write refused **engine-side**. Also: verify-full without the CA is refused, and SCRAM-only
//! reaches the driver.
//!
//! Needs Docker (an OpenSSH container on the host network, `127.0.0.1` only) and the dev
//! PostgreSQL (`python3 dev/ready.py up core`). Skipped (not failed) when either is missing,
//! unless `NINESQL_REQUIRE_ENGINES` lists `ssh` or `postgresql` (AGENTS.md). The container is
//! removed at the end, also on failure; the test schema is dropped.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stderr)] // Tests may panic (AGENTS.md).

mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use ninesql_core::driver_api::arrow_array::{Array, StringArray};
use ninesql_core::driver_api::{DriverError, ResultItem};
use ninesql_core::{
    Auth, Connection, ConnectionTypeId, ConnectionTypes, Connections, CoreConfig, CoreError,
    CoreServices, DriverRegistry, Endpoint, EngineId, ExecEvent, HostKeyPolicy, KnownHostsFiles,
    OpenOutcome, OpenRequest, PasswordExchange, SecretSlot, SecretSource, SshAuth, SshHop,
    SshTunnel, TlsMode,
};
use ninesql_driver_pgwire::PgWireDriver;
use ninesql_secrets::{KdfParams, SecretString};
use ninesql_storage::{Paths, ProjectName, XdgDirs};
use support::*;

struct Dev {
    host: String,
    port: u16,
    user: String,
    password: String,
    database: String,
    ca_sql: String,
}

fn required(engine: &str) -> bool {
    std::env::var("NINESQL_REQUIRE_ENGINES")
        .unwrap_or_default()
        .split([',', ' '])
        .any(|e| e.trim() == engine || e.trim() == "all")
}

/// Dev PostgreSQL parameters from `dev/engines.json`, or `None` (skipped) when unreachable.
fn dev() -> Option<Dev> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../dev/engines.json");
    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let e = doc["engines"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == "postgresql")
        .unwrap();
    let dev = Dev {
        host: e["host"].as_str().unwrap().to_owned(),
        port: u16::try_from(e["port"].as_u64().unwrap()).unwrap(),
        user: e["user"].as_str().unwrap().to_owned(),
        password: e["password"].as_str().unwrap().to_owned(),
        database: e["database"].as_str().unwrap().to_owned(),
        ca_sql: e["tls"]["ca_sql"].as_str().unwrap().to_owned(),
    };
    let addr = format!("{}:{}", dev.host, dev.port);
    let reachable = addr
        .parse()
        .ok()
        .and_then(|a| std::net::TcpStream::connect_timeout(&a, Duration::from_secs(2)).ok())
        .is_some();
    if reachable {
        return Some(dev);
    }
    assert!(
        !required("postgresql"),
        "postgresql is required but unreachable at {addr}"
    );
    eprintln!("skipped: postgresql unreachable at {addr} (python3 dev/ready.py up core)");
    None
}

fn paths(root: &Path) -> Paths {
    Paths::new(&XdgDirs {
        config_home: root.join("config"),
        data_home: root.join("data"),
        state_home: root.join("state"),
    })
}

fn pg_connection(dev: &Dev, host: &str) -> Connection {
    Connection::new(
        "Dev PostgreSQL",
        EngineId::new("postgresql").unwrap(),
        Endpoint::Tcp {
            host: host.to_owned(),
            port: Some(dev.port),
            database: Some(dev.database.clone()),
        },
        Auth::Password {
            username: dev.user.clone(),
            secret: SecretSource::Prompt,
        },
        ConnectionTypeId::dev(),
    )
}

/// The first text value of the first row, from an execution's events.
fn scalar_text(events: &[ExecEvent]) -> Option<String> {
    events.iter().find_map(|e| match e {
        ExecEvent::Item(ResultItem::Rows(b)) if b.num_rows() > 0 => {
            let col = b.column(0).as_any().downcast_ref::<StringArray>()?;
            (!col.is_null(0)).then(|| col.value(0).to_owned())
        }
        _ => None,
    })
}

fn finished(events: &[ExecEvent]) -> bool {
    matches!(terminal(events), ExecEvent::Finished { .. })
}

#[test]
fn strict_tunnel_verify_full_read_only_session_refuses_writes_engine_side() {
    let Some(dev) = dev() else { return };
    let ssh_password = format!("ssh-{}", uuid::Uuid::new_v4().simple());
    let Some(sshd) = support::sshd::start_host_network(&ssh_password) else {
        return;
    };
    let schema = format!("ninesql_core_it_{}", uuid::Uuid::new_v4().simple());
    let master = format!("master-{}", uuid::Uuid::new_v4().simple());

    let tmp = tempfile::tempdir().unwrap();
    let paths = paths(tmp.path());
    let mut registry = DriverRegistry::new();
    registry.register(Arc::new(PgWireDriver::new()));
    let mut config = CoreConfig::new(paths.clone(), ProjectName::default_project(), registry);
    config.manager.known_hosts = KnownHostsFiles {
        own: paths.data_dir().join("known_hosts"),
        user: None,
    };
    config.manager.workers = 2;
    config.use_secret_service = false;
    config.watch_desktop_lock = false;
    let services = CoreServices::start_blocking(config).unwrap();
    let types = ConnectionTypes::default();
    let sessions = services.sessions().clone();
    let db_secret = || SecretString::from(dev.password.clone());
    let ssh_secret = || SecretString::from(ssh_password.clone());

    // A writable, direct session: reads the dev CA (public) and creates a function that writes,
    // which no client-side classifier can see as a write.
    let mut plain = pg_connection(&dev, &dev.host);
    plain.tls.mode = TlsMode::Disable;
    let ca_path = tmp.path().join("dev-ca.pem");
    ui(async {
        let req =
            OpenRequest::new(plain.clone(), &types).with_secret(SecretSlot::Auth, db_secret());
        let OpenOutcome::Opened(h) = open(&sessions, req).await.unwrap() else {
            panic!("plain session not opened");
        };
        let ca = scalar_text(&run(&h, &dev.ca_sql).await).expect("dev CA");
        assert!(ca.contains("BEGIN CERTIFICATE"));
        std::fs::write(&ca_path, ca).unwrap();
        for sql in [
            format!("CREATE SCHEMA {schema}"),
            format!("CREATE TABLE {schema}.written (id int)"),
            format!(
                "CREATE FUNCTION {schema}.write_one() RETURNS int LANGUAGE sql AS \
                 'INSERT INTO {schema}.written VALUES (1) RETURNING 1'"
            ),
        ] {
            let ev = run(&h, &sql).await;
            assert!(finished(&ev), "{sql}: {:?}", terminal(&ev));
        }
        h.close().await.unwrap().unwrap();
    });

    // The tunneled connection: the password saved in the vault, SCRAM only, verify-full with the
    // dev CA, a strict SSH hop with a prompted password, read-only.
    ui(async {
        services
            .create_vault_with(
                SecretString::from(master.clone()),
                KdfParams::insecure_for_tests(),
            )
            .await
            .unwrap()
            .unwrap();
    });
    let mut tunneled = pg_connection(&dev, "localhost");
    tunneled.name = "Dev PostgreSQL through SSH".into();
    tunneled.read_only = Some(true);
    tunneled.password_exchange = PasswordExchange::ScramOnly;
    tunneled.tls.mode = TlsMode::VerifyFull;
    tunneled.ssh = Some(SshTunnel {
        server: SshHop::new(
            "127.0.0.1",
            sshd.port,
            support::sshd::USER,
            SshAuth::Password {
                secret: SecretSource::Prompt,
            },
        ),
        jump_hosts: vec![],
    });
    assert_eq!(
        tunneled.ssh.as_ref().unwrap().server.host_key_policy,
        HostKeyPolicy::Strict
    );
    let mut connections = Connections::default();
    let id = services
        .workspace()
        .create_connection_checked(
            &mut connections,
            tunneled,
            vec![(SecretSlot::Auth, db_secret())],
            services.secret_backend().store().unwrap(),
            services.secret_stores(),
            services.registry(),
        )
        .unwrap();
    let saved = connections.get(id).unwrap().clone();
    let mut with_ca = saved.clone();
    with_ca.tls.ca_file = Some(ca_path.clone());
    services.validate_connection(&with_ca).unwrap();

    ui(async {
        let request = |c: &Connection| {
            OpenRequest::new(c.clone(), &types).with_secret(SecretSlot::SshServer, ssh_secret())
        };
        // The vault is locked: asked first, nothing dialled.
        services.lock_now().await.unwrap();
        let out = open(&sessions, request(&saved)).await.unwrap();
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
        services
            .unlock_vault(SecretString::from(master.clone()))
            .await
            .unwrap()
            .unwrap();

        // Strict: the unknown host key is confirmed before any credential is sent.
        let out = open(&sessions, request(&saved)).await.unwrap();
        let OpenOutcome::HostKeyNeedsConfirmation(confirm) = out else {
            panic!("{out:?}");
        };
        assert_eq!((confirm.key.hop, confirm.key.port), (0, sshd.port));
        sessions
            .trust_host_key(confirm.token)
            .await
            .unwrap()
            .unwrap();

        // verify-full without the dev CA: the throwaway CA is not trusted, the connect fails.
        let err = open(&sessions, request(&saved)).await.unwrap_err();
        assert!(
            matches!(err, CoreError::Driver(DriverError::Tls(_))),
            "{err:?}"
        );

        // With the dev CA: verify-full passes through the tunnel (server_name = localhost).
        let out = open(&sessions, request(&with_ca)).await.unwrap();
        let OpenOutcome::Opened(h) = out else {
            panic!("{out:?}");
        };
        let info = h.info();
        assert!(info.read_only, "read-only session");
        let route = info.tunnel.as_ref().expect("tunnel route");
        assert_eq!(
            (route.ssh_host.as_str(), route.ssh_port),
            ("127.0.0.1", sshd.port)
        );
        assert!(info.warnings.is_empty(), "{:?}", info.warnings);

        // Reads work.
        let ev = run(
            &h,
            "SELECT current_setting('default_transaction_read_only')",
        )
        .await;
        assert_eq!(scalar_text(&ev).as_deref(), Some("on"));
        let ev = run(
            &h,
            "SELECT ssl::text FROM pg_stat_ssl WHERE pid = pg_backend_pid()",
        )
        .await;
        assert_eq!(
            scalar_text(&ev).as_deref(),
            Some("true"),
            "TLS through the tunnel"
        );

        // A write the classifier cannot see is refused by the server (I3).
        let ev = run(&h, &format!("SELECT {schema}.write_one()::text")).await;
        match terminal(&ev) {
            ExecEvent::Failed(CoreError::Driver(DriverError::ReadOnly(msg))) => {
                assert!(msg.contains("read-only"), "{msg}");
            }
            other => panic!("engine-side write not refused: {other:?}"),
        }
        let ev = run(&h, &format!("CREATE TABLE {schema}.nope (i int)")).await;
        assert!(
            matches!(terminal(&ev), ExecEvent::Failed(_)),
            "{:?}",
            terminal(&ev)
        );
        let ev = run(&h, &format!("SELECT count(*)::text FROM {schema}.written")).await;
        assert_eq!(scalar_text(&ev).as_deref(), Some("0"), "nothing written");
        h.close().await.unwrap().unwrap();
    });

    // Cleanup through the writable session.
    ui(async {
        let req =
            OpenRequest::new(plain.clone(), &types).with_secret(SecretSlot::Auth, db_secret());
        if let Ok(OpenOutcome::Opened(h)) = open(&sessions, req).await {
            let _ = run(&h, &format!("DROP SCHEMA {schema} CASCADE")).await;
            let _ = h.close().await;
        }
    });
    services.shutdown(Duration::from_secs(5));
    drop(sshd);
}
