//! Required by AGENTS.md (I1): no password reaches any file under the NineSQL directories after
//! the create / save / load / remove flows. Also checks `0700` / `0600` on everything written.
#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).

use std::fmt::Write as _;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ninesql_core::{
    Auth, ClientCertificate, Connection, ConnectionTypeId, ConnectionTypes, Connections, Endpoint,
    EngineId, SecretSlot, SecretSource, SshAuth, SshHop, SshTunnel, Workspace,
};
use ninesql_secrets::{ExposeSecret, MemoryStore, SecretStores, SecretString, StoreKind};
use ninesql_storage::{Paths, ProjectName, XdgDirs};
use proptest::prelude::*;

/// Minimal tracing subscriber that records every span and event field, at every level, as text.
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

impl Capture {
    fn text(&self) -> String {
        self.0.lock().unwrap().clone()
    }
}

/// Runs `f` with every tracing event captured, then checks no secret reached the logs.
fn without_secret_in_logs<T>(secrets: &[&str], f: impl FnOnce() -> T) -> T {
    let capture = Capture::default();
    let out = tracing::subscriber::with_default(capture.clone(), f);
    let logs = capture.text();
    assert!(!logs.is_empty(), "the capture saw no event at all");
    for secret in secrets {
        for form in forms(secret) {
            assert!(!logs.contains(&form), "a secret reached the logs");
        }
    }
    out
}

/// The raw secret and its JSON-escaped form (quotes, backslashes, control characters).
fn forms(secret: &str) -> Vec<String> {
    let json = serde_json::to_string(secret).unwrap();
    let escaped = json[1..json.len() - 1].to_owned();
    let mut forms = vec![secret.to_owned()];
    if escaped != secret {
        forms.push(escaped);
    }
    forms
}

struct Env {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    workspace: Workspace,
    stores: SecretStores,
    keyring: Arc<MemoryStore>,
}

fn env() -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    let xdg = XdgDirs {
        config_home: root.join("config"),
        data_home: root.join("data"),
        state_home: root.join("state"),
    };
    let workspace = Workspace::open(Paths::new(&xdg), ProjectName::default_project()).unwrap();
    let keyring = Arc::new(MemoryStore::new(StoreKind::Keyring));
    let mut stores = SecretStores::new();
    stores.register(keyring.clone());
    Env {
        _tmp: tmp,
        root,
        workspace,
        stores,
        keyring,
    }
}

fn full_connection() -> Connection {
    let mut c = Connection::new(
        "Orders",
        EngineId::new("postgresql").unwrap(),
        Endpoint::Tcp {
            host: "db.example.test".into(),
            port: Some(5432),
            database: Some("orders".into()),
        },
        Auth::Password {
            username: "app".into(),
            secret: SecretSource::Prompt,
        },
        ConnectionTypeId::prod(),
    );
    c.ssh = Some(SshTunnel {
        server: SshHop {
            host: "bastion.example.test".into(),
            port: 22,
            username: "deploy".into(),
            auth: SshAuth::Password {
                secret: SecretSource::Prompt,
            },
        },
        jump_hosts: vec![SshHop {
            host: "edge.example.test".into(),
            port: 22,
            username: "jump".into(),
            auth: SshAuth::PublicKey {
                key_path: "/home/u/.ssh/id_ed25519".into(),
                passphrase: None,
            },
        }],
    });
    c.tls.client_certificate = Some(ClientCertificate {
        cert_file: "/home/u/client.pem".into(),
        key_file: "/home/u/client.key".into(),
        passphrase: None,
    });
    c
}

/// Every regular file under `root`, recursively.
fn all_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
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

fn assert_no_secret_on_disk(root: &Path, secrets: &[&str]) {
    let files = all_files(root);
    assert!(!files.is_empty());
    for file in files {
        let bytes = fs::read(&file).unwrap();
        for secret in secrets {
            assert!(
                !bytes.windows(secret.len()).any(|w| w == secret.as_bytes()),
                "a secret reached {}",
                file.display()
            );
        }
    }
}

fn assert_private_tree(dir: &Path) {
    let mode = fs::metadata(dir).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700, "{}", dir.display());
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            assert_private_tree(&path);
        } else {
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{}", path.display());
        }
    }
}

const DB_PASSWORD: &str = "db-Pa55w0rd-6f1c2a";
const SSH_PASSWORD: &str = "ssh-Pa55w0rd-0e9d3b";
const KEY_PASSPHRASE: &str = "key-Pa55phrase-a71e44";
const TLS_PASSPHRASE: &str = "tls-Pa55phrase-55c0d2";

fn secrets() -> Vec<(SecretSlot, SecretString)> {
    vec![
        (SecretSlot::Auth, SecretString::from(DB_PASSWORD.to_owned())),
        (
            SecretSlot::SshServer,
            SecretString::from(SSH_PASSWORD.to_owned()),
        ),
        (
            SecretSlot::SshJump(0),
            SecretString::from(KEY_PASSPHRASE.to_owned()),
        ),
        (
            SecretSlot::TlsClientKey,
            SecretString::from(TLS_PASSPHRASE.to_owned()),
        ),
    ]
}

#[test]
fn create_save_load_remove_never_writes_a_secret() {
    let all = [DB_PASSWORD, SSH_PASSWORD, KEY_PASSPHRASE, TLS_PASSPHRASE];
    without_secret_in_logs(&all, create_save_load_remove);
}

fn create_save_load_remove() {
    let env = env();
    let mut connections = Connections::default();
    let id = env
        .workspace
        .create_connection(
            &mut connections,
            full_connection(),
            secrets(),
            StoreKind::Keyring,
            &env.stores,
        )
        .unwrap();
    env.workspace
        .save_connection_types(&ConnectionTypes::default())
        .unwrap();

    // Secrets are in the store, behind the references written to the file.
    assert_eq!(env.keyring.len(), 4);
    let loaded = env.workspace.load_connections().unwrap();
    assert!(loaded.issues.is_empty());
    let loaded = loaded.connections;
    assert_eq!(loaded, connections);
    let c = loaded.get(id).unwrap();
    let resolved: Vec<String> = c
        .secret_refs()
        .iter()
        .map(|(_, _, r)| env.stores.resolve(r).unwrap().expose_secret().to_owned())
        .collect();
    assert_eq!(
        resolved,
        [DB_PASSWORD, SSH_PASSWORD, KEY_PASSPHRASE, TLS_PASSPHRASE]
    );

    // Nothing in Debug output either (it could end up in a log or a crash report).
    let debug = format!("{c:?} {connections:?} {:?}", env.stores);
    for s in [DB_PASSWORD, SSH_PASSWORD, KEY_PASSPHRASE, TLS_PASSPHRASE] {
        assert!(!debug.contains(s));
    }

    let all = [DB_PASSWORD, SSH_PASSWORD, KEY_PASSPHRASE, TLS_PASSPHRASE];
    assert_no_secret_on_disk(&env.root, &all);
    assert_private_tree(&env.root.join("config/ninesql"));
    assert_private_tree(&env.root.join("data/ninesql"));
    assert_private_tree(&env.root.join("state/ninesql"));

    // Removing deletes the secrets from the store, and still nothing on disk.
    let mut connections = loaded;
    env.workspace
        .remove_connection(&mut connections, id, &env.stores)
        .unwrap();
    assert!(env.keyring.is_empty());
    assert!(
        env.workspace
            .load_connections()
            .unwrap()
            .connections
            .is_empty()
    );
    assert_no_secret_on_disk(&env.root, &all);
}

#[test]
fn bad_slot_stores_nothing() {
    let env = env();
    let mut connections = Connections::default();
    let mut c = full_connection();
    c.auth = Auth::None;
    let err = env
        .workspace
        .create_connection(
            &mut connections,
            c,
            vec![(SecretSlot::Auth, SecretString::from(DB_PASSWORD.to_owned()))],
            StoreKind::Keyring,
            &env.stores,
        )
        .unwrap_err();
    assert!(err.to_string().contains("no credential field"));
    assert!(env.keyring.is_empty());
    assert!(connections.is_empty());
    assert!(!env.workspace.project_paths().connections_file().exists());
}

#[test]
fn missing_store_stores_nothing_and_writes_nothing() {
    let env = env();
    let mut connections = Connections::default();
    let err = env
        .workspace
        .create_connection(
            &mut connections,
            full_connection(),
            secrets(),
            StoreKind::Vault, // not registered
            &env.stores,
        )
        .unwrap_err();
    assert!(err.to_string().contains("vault"));
    assert!(env.keyring.is_empty());
    assert!(connections.is_empty());
    assert!(!env.workspace.project_paths().connections_file().exists());
}

#[test]
fn file_save_failure_rolls_back_stored_secrets() {
    let env = env();
    // A directory where connections.json should go makes the atomic rename fail.
    let target = env.workspace.project_paths().connections_file();
    fs::create_dir(&target).unwrap();
    fs::write(target.join("block"), b"").unwrap();
    let mut connections = Connections::default();
    assert!(
        env.workspace
            .create_connection(
                &mut connections,
                full_connection(),
                secrets(),
                StoreKind::Keyring,
                &env.stores,
            )
            .is_err()
    );
    assert!(env.keyring.is_empty());
    assert!(connections.is_empty());
}

#[test]
fn secret_like_driver_property_is_refused() {
    let env = env();
    let mut connections = Connections::default();
    let mut c = full_connection();
    c.properties
        .insert("sslpassword".into(), DB_PASSWORD.into());
    let err = env
        .workspace
        .create_connection(
            &mut connections,
            c.clone(),
            Vec::new(),
            StoreKind::Keyring,
            &env.stores,
        )
        .unwrap_err();
    assert!(!err.to_string().contains(DB_PASSWORD));
    // And save_connections refuses it too, writing nothing.
    let set = Connections::from_vec(vec![c]).unwrap();
    assert!(env.workspace.save_connections(&set).is_err());
    assert_no_secret_on_disk(&env.root, &[DB_PASSWORD]);
}

/// A hand-edited `connections.json` holding secrets in unknown keys, driver properties and init
/// SQL: the load keeps the project usable, reports the connection, and rewrites the file at once
/// (N7.2) so no secret is left on disk or in the logs, before any save. (`connections.json` has no
/// older schema yet; its `.bak` scrub is covered by `connections_backup_scrub_uses_the_load_rules`
/// below and in `ninesql-storage/tests/migrations.rs`.)
#[test]
fn hand_edited_secrets_are_reported_and_dropped() {
    const SECRET: &str = "Hand\"Edited\\Secret-9b2e";
    let env = env();
    let good = Connection::new(
        "Good",
        EngineId::new("sqlite").unwrap(),
        Endpoint::File {
            path: "/tmp/g.db".into(),
        },
        Auth::None,
        ConnectionTypeId::dev(),
    );
    let mut bad = serde_json::to_value(full_connection()).unwrap();
    bad["auth"]["password"] = SECRET.into();
    bad["properties"] = serde_json::json!({
        "sslpassword": SECRET,
        "url": format!("postgres://app:{SECRET}@db/x"),
        "application_name": "NineSQL"
    });
    bad["init_sql"] = serde_json::json!([format!("ALTER USER app PASSWORD '{SECRET}'")]);
    let file = serde_json::json!({ "schema_version": 1, "connections": [good, bad] });
    let path = env.workspace.project_paths().connections_file();
    fs::write(&path, serde_json::to_vec_pretty(&file).unwrap()).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

    let loaded = without_secret_in_logs(&[SECRET], || env.workspace.load_connections().unwrap());
    assert_eq!(loaded.connections.len(), 2);
    assert_eq!(loaded.issues.len(), 3, "{:?}", loaded.issues);
    assert_eq!(
        loaded.secret_like_keys,
        [
            "connections[1].auth.password",
            "connections[1].properties.sslpassword"
        ]
    );
    assert!(!format!("{:?}", loaded.issues).contains("Hand"));
    // Already gone from disk, without waiting for a save; the file stays private.
    assert_no_secret_on_disk(&env.root, &[SECRET]);
    assert_private_tree(&env.root.join("data"));
    // The rewritten file loads cleanly.
    let again = env.workspace.load_connections().unwrap();
    assert!(again.issues.is_empty(), "{:?}", again.issues);
    assert!(again.secret_like_keys.is_empty());
    assert_eq!(again.connections, loaded.connections);

    without_secret_in_logs(&[SECRET], || {
        env.workspace.save_connections(&loaded.connections).unwrap();
    });
    assert_no_secret_on_disk(&env.root, &[SECRET]);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]
    /// Whatever the password, it ends up in the store and never in a file.
    #[test]
    fn arbitrary_passwords_never_reach_disk(password in "[!-~]{12,48}") {
        // `forms` also checks the JSON-escaped form (quotes and backslashes are in the range).
        let env = env();
        let mut connections = Connections::default();
        env.workspace
            .create_connection(
                &mut connections,
                full_connection(),
                vec![(SecretSlot::Auth, SecretString::from(password.clone()))],
                StoreKind::Keyring,
                &env.stores,
            )
            .unwrap();
        assert_no_secret_on_disk(&env.root, &[password.as_str()]);
    }
}

/// `.bak` scrub: the hook applies the load rules to the raw file, so a migration backup of
/// `connections.json` never copies a secret held in a driver property or init SQL.
#[test]
fn connections_backup_scrub_uses_the_load_rules() {
    use ninesql_storage::Document;
    const SECRET: &str = "Bak-Secret-77c1";
    let mut c = serde_json::to_value(full_connection()).unwrap();
    c["properties"] = serde_json::json!({
        "application_name": "NineSQL",
        "sslpassword": SECRET,
        "url": format!("postgres://app:{SECRET}@db/x"),
        "connect_timeout": format!("user:{SECRET}@tcp(db)/x"),
        "charset": 42,
        "passwordCharacterEncoding": SECRET,
    });
    c["init_sql"] = serde_json::json!([
        "SET search_path TO app",
        format!("SET PASSWORD = PASSWORD('{SECRET}')"),
        7,
    ]);
    let mut file = serde_json::json!({ "schema_version": 1, "connections": [c] });
    ninesql_core::ConnectionsFile::scrub(&mut file, 1);
    let text = file.to_string();
    assert!(!text.contains(SECRET), "{text}");
    assert_eq!(
        file["connections"][0]["properties"],
        serde_json::json!({ "application_name": "NineSQL" })
    );
    assert_eq!(
        file["connections"][0]["init_sql"],
        serde_json::json!(["SET search_path TO app"])
    );
    // Other layouts are left alone.
    let mut other = serde_json::json!({ "schema_version": 1, "something": [1, 2] });
    let before = other.clone();
    ninesql_core::ConnectionsFile::scrub(&mut other, 1);
    assert_eq!(other, before);
}
