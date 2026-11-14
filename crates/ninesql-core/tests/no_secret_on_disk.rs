//! Required by AGENTS.md (I1): no password reaches any file under the NineSQL directories after
//! the create / save / load / remove flows. Also checks `0700` / `0600` on everything written.
#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ninesql_core::{
    Auth, ClientCertificate, Connection, ConnectionTypeId, ConnectionTypes, Connections, Endpoint,
    EngineId, SecretSlot, SecretSource, SshAuth, SshHop, SshTunnel, Workspace,
};
use ninesql_secrets::{ExposeSecret, MemoryStore, SecretStores, SecretString, StoreKind};
use ninesql_storage::{Paths, ProjectName, XdgDirs};
use proptest::prelude::*;

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
    assert!(env.workspace.load_connections().unwrap().is_empty());
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

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]
    /// Whatever the password, it ends up in the store and never in a file.
    #[test]
    fn arbitrary_passwords_never_reach_disk(password in "[!-~]{12,48}") {
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
