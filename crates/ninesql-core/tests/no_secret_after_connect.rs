//! I1 (AGENTS.md "Required tests"): after create / connect / execute / close flows through the
//! driver runtime, no secret is in any file under the NineSQL directories nor in any log event,
//! at any level, on any thread. Also checks that production query text never reaches the logs
//! (ADR-0007 §6) while development query text does at `trace` level (proving the capture sees
//! it).
//!
//! One test per process: the log capture is the global subscriber, so the runtime's worker
//! threads are captured too.
#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).

mod support;

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ninesql_core::driver_api::Secret;
use ninesql_core::driver_api::mock::MockDriver;
use ninesql_core::{
    Auth, ConnectionTypeId, ConnectionTypes, Connections, CoreError, ExecEvent, OpenOutcome,
    OpenRequest, SecretSlot, SecretSource, SshAuth, SshHop, SshTunnel, Workspace,
};
use ninesql_secrets::{SecretStores, SecretString, StoreKind};
use ninesql_storage::{Paths, ProjectName, XdgDirs};
use support::*;

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
        buf.push_str(event.metadata().target());
        event.record(&mut Visitor(&mut buf));
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
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

const DB_PASSWORD: &str = "Db-Pa55-7f3a9c";
const PROMPTED_PASSWORD: &str = "Pr0mpt-Pa55-4e1b";
const SSH_PASSWORD: &str = "Ssh-Pa55-91d2";
const PROD_MARKER: &str = "prod_marker_5b7e";
const DEV_MARKER: &str = "dev_marker_c3a1";

#[test]
fn no_secret_in_workspace_files_or_logs_after_connect_flows() {
    let capture = Capture::default();
    tracing::subscriber::set_global_default(capture.clone()).unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    let xdg = XdgDirs {
        config_home: root.join("config"),
        data_home: root.join("data"),
        state_home: root.join("state"),
    };
    let paths = Paths::new(&xdg);
    let ws = Workspace::open(paths.clone(), ProjectName::default_project()).unwrap();
    let wrap = Wrap::new(MockDriver::new().with_password(Secret::new(DB_PASSWORD.into())));
    // `known_hosts` under the NineSQL data directory, so the scan covers it.
    let (manager, keyring) = manager(Arc::new(wrap), paths.data_dir());
    let mut stores = SecretStores::new();
    stores.register(keyring.clone());
    let types = ConnectionTypes::default();

    // Create: the password goes to the keyring, the reference to `connections.json`.
    let mut connections = Connections::default();
    let mut dev = mock_connection(ConnectionTypeId::dev());
    dev.auth = Auth::Password {
        username: "app".into(),
        secret: SecretSource::Prompt,
    };
    let dev_id = ws
        .create_connection(
            &mut connections,
            dev,
            vec![(SecretSlot::Auth, SecretString::from(DB_PASSWORD.to_owned()))],
            StoreKind::Keyring,
            &stores,
        )
        .unwrap();
    let mut prod = mock_connection(ConnectionTypeId::prod());
    prod.id = ninesql_core::ConnectionId::new_random();
    prod.auth = Auth::Password {
        username: "app".into(),
        secret: SecretSource::Prompt,
    };
    let prod_id = prod.id;
    connections.insert(prod).unwrap();
    // An SSH connection whose tunnel cannot open: the SSH password must not leak either.
    let mut ssh = mock_connection(ConnectionTypeId::dev());
    ssh.id = ninesql_core::ConnectionId::new_random();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    ssh.ssh = Some(SshTunnel {
        server: SshHop::new(
            "127.0.0.1",
            port,
            "deploy",
            SshAuth::Password {
                secret: SecretSource::Prompt,
            },
        ),
        jump_hosts: vec![],
    });
    let ssh_id = ssh.id;
    connections.insert(ssh).unwrap();
    ws.save_connections(&connections).unwrap();

    ui(async {
        // Saved secret: connect, run, close.
        let dev = connections.get(dev_id).unwrap().clone();
        let h = open_ok(&manager, dev, &types).await;
        let ev = run(&h, &format!("SELECT 1 AS {DEV_MARKER}")).await;
        assert!(
            matches!(terminal(&ev), ExecEvent::Finished { .. }),
            "{ev:?}"
        );
        let ev = run(&h, "SELECT * FROM numbers(10)").await;
        assert!(matches!(terminal(&ev), ExecEvent::Finished { .. }));
        h.close().await.unwrap().unwrap();

        // Prompted secret on production: a wrong one, then the right one.
        let prod = connections.get(prod_id).unwrap().clone();
        let wrong = OpenRequest::new(prod.clone(), &types).with_secret(
            SecretSlot::Auth,
            SecretString::from(PROMPTED_PASSWORD.to_owned()),
        );
        assert!(open(&manager, wrong).await.is_err());
        let right = OpenRequest::new(prod, &types)
            .with_secret(SecretSlot::Auth, SecretString::from(DB_PASSWORD.to_owned()));
        let Ok(OpenOutcome::Opened(h)) = open(&manager, right).await else {
            panic!("production connection did not open");
        };
        let ev = run(&h, &format!("SELECT 1 AS {PROD_MARKER}")).await;
        assert!(
            matches!(terminal(&ev), ExecEvent::Finished { .. }),
            "{ev:?}"
        );
        let ev = run(&h, &format!("DROP TABLE {PROD_MARKER}")).await;
        assert!(matches!(terminal(&ev), ExecEvent::NeedsConfirmation(_)));
        h.close().await.unwrap().unwrap();

        // SSH tunnel failure with a prompted SSH password.
        let ssh = connections.get(ssh_id).unwrap().clone();
        let req = OpenRequest::new(ssh, &types).with_secret(
            SecretSlot::SshServer,
            SecretString::from(SSH_PASSWORD.to_owned()),
        );
        assert!(matches!(open(&manager, req).await, Err(CoreError::Ssh(_))));
    });
    manager.shutdown(Duration::from_secs(5));
    // Workspace saved again after the flows (as the UI would on exit).
    ws.save_connections(&connections).unwrap();

    let secrets = [DB_PASSWORD, PROMPTED_PASSWORD, SSH_PASSWORD];
    let files = all_files(&root);
    assert!(files.iter().any(|f| f.ends_with("connections.json")));
    for file in &files {
        let bytes = std::fs::read(file).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        for secret in secrets {
            assert!(
                !text.contains(secret),
                "a secret reached {}",
                file.display()
            );
        }
    }
    let logs = capture.0.lock().unwrap().clone();
    assert!(
        logs.contains("session opened"),
        "the capture saw the runtime's events"
    );
    for secret in secrets {
        assert!(!logs.contains(secret), "a secret reached the logs");
    }
    assert!(
        logs.contains(DEV_MARKER),
        "dev query text is logged at trace level"
    );
    assert!(
        !logs.contains(PROD_MARKER),
        "production query text reached the logs"
    );
}
