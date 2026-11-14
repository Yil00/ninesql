//! On-disk format of `connections.json` and `connection-types.json`, locked by versioned fixtures.
#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).

use std::path::{Path, PathBuf};

use ninesql_core::{
    Auth, ConnectionTypeId, ConnectionTypes, ConnectionTypesFile, ConnectionsFile, Endpoint,
    Origin, SecretSource, SecurityWarning, SshAuth, TlsMode,
};
use ninesql_secrets::{SecretKind, StoreKind};
use ninesql_storage::document;
use serde_json::Value;

fn fixture(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(rel)
}

fn decode_connections(rel: &str) -> ConnectionsFile {
    let path = fixture(rel);
    let bytes = std::fs::read(&path).unwrap();
    document::decode::<ConnectionsFile>(&path, &bytes)
        .unwrap()
        .document
}

#[test]
fn connections_v1_fixture_loads_with_every_field() {
    let file = decode_connections("connections/v1.json");
    assert_eq!(file.connections.len(), 5);
    for c in &file.connections {
        c.validate().unwrap();
    }

    let prod = &file.connections[0];
    assert_eq!(prod.engine.as_str(), "postgresql");
    assert!(matches!(
        &prod.endpoint,
        Endpoint::Tcp {
            port: Some(5432),
            ..
        }
    ));
    assert_eq!(prod.connection_type, ConnectionTypeId::prod());
    assert_eq!(prod.read_only, Some(true));
    assert_eq!(prod.folder, ["Customers", "EU"]);
    assert_eq!(prod.tls.mode, TlsMode::VerifyFull);
    assert!(prod.security_warnings().is_empty());
    let ssh = prod.ssh.as_ref().unwrap();
    assert_eq!(ssh.server.port, 2222);
    assert!(matches!(
        ssh.server.auth,
        SshAuth::PublicKey {
            passphrase: Some(_),
            ..
        }
    ));
    assert!(matches!(ssh.jump_hosts[0].auth, SshAuth::Agent));
    let kinds: Vec<_> = prod.secret_refs().iter().map(|(_, k, _)| *k).collect();
    assert_eq!(kinds, [SecretKind::Password, SecretKind::SshKeyPassphrase]);
    assert!(matches!(
        prod.tls.client_certificate.as_ref().unwrap().passphrase,
        Some(SecretSource::Prompt)
    ));

    let mysql = &file.connections[1];
    assert!(
        matches!(&mysql.origin, Origin::Dbeaver { dbeaver_id, .. } if dbeaver_id.starts_with("mysql8"))
    );
    assert_eq!(mysql.security_warnings(), [SecurityWarning::TlsDisabled]);
    assert!(mysql.secret_refs().is_empty());

    let duck = &file.connections[2];
    assert!(!duck.enabled);
    assert!(matches!(duck.auth, Auth::None));

    assert_eq!(file.connections[3].auth.username(), Some("postgres"));

    let redis = &file.connections[4];
    assert_eq!(
        redis.security_warnings(),
        [SecurityWarning::TlsCertificateNotVerified]
    );
    assert_eq!(redis.secret_refs()[0].2.store, StoreKind::Vault);
}

#[test]
fn connections_v1_round_trip_is_stable() {
    let path = fixture("connections/v1.json");
    let original: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let file = decode_connections("connections/v1.json");
    let encoded: Value = serde_json::from_slice(&document::encode(&file).unwrap()).unwrap();
    assert_eq!(encoded, original);
}

#[test]
fn missing_tls_means_verify_full() {
    let json = br#"{ "schema_version": 1, "connections": [ {
        "id": "0b0c6c0e-6f0a-4d4e-9c47-1f1e0f3a9a99", "name": "n", "engine": "postgresql",
        "endpoint": { "kind": "tcp", "host": "h" }, "auth": { "method": "none" },
        "connection_type": "dev"
    } ] }"#;
    let file = document::decode::<ConnectionsFile>(Path::new("mem"), json)
        .unwrap()
        .document;
    let c = &file.connections[0];
    assert_eq!(c.tls.mode, TlsMode::VerifyFull);
    assert!(c.enabled);
    assert_eq!(c.origin, Origin::Native);
    // The default is written explicitly on save, so the file shows it.
    let text = String::from_utf8(document::encode(&file).unwrap()).unwrap();
    assert!(text.contains("\"verify-full\""));
}

#[test]
fn connection_type_is_required() {
    let json = br#"{ "schema_version": 1, "connections": [ {
        "id": "0b0c6c0e-6f0a-4d4e-9c47-1f1e0f3a9a99", "name": "n", "engine": "postgresql",
        "endpoint": { "kind": "tcp", "host": "h" }, "auth": { "method": "none" }
    } ] }"#;
    assert!(document::decode::<ConnectionsFile>(Path::new("mem"), json).is_err());
}

#[test]
fn unknown_fields_are_tolerated_and_dropped() {
    let json = br#"{ "schema_version": 1, "future": 1, "connections": [ {
        "id": "0b0c6c0e-6f0a-4d4e-9c47-1f1e0f3a9a99", "name": "n", "engine": "postgresql",
        "endpoint": { "kind": "tcp", "host": "h", "future_endpoint_option": true },
        "auth": { "method": "password", "username": "u", "secret": { "source": "prompt" },
                  "password": "hunter2" },
        "connection_type": "dev", "colour_hint": "blue"
    } ] }"#;
    let loaded = document::decode::<ConnectionsFile>(Path::new("mem"), json).unwrap();
    assert_eq!(loaded.secret_like_keys, ["connections[0].auth.password"]);
    let text = String::from_utf8(document::encode(&loaded.document).unwrap()).unwrap();
    assert!(!text.contains("hunter2"));
    assert!(!text.contains("colour_hint"));
}

#[test]
fn secret_reference_with_extra_fields_is_rejected() {
    let json = br#"{ "schema_version": 1, "connections": [ {
        "id": "0b0c6c0e-6f0a-4d4e-9c47-1f1e0f3a9a99", "name": "n", "engine": "postgresql",
        "endpoint": { "kind": "tcp", "host": "h" },
        "auth": { "method": "password", "username": "u",
                  "secret": { "source": "stored", "store": "keyring", "id": "x", "value": "hunter2" } },
        "connection_type": "dev"
    } ] }"#;
    let err = document::decode::<ConnectionsFile>(Path::new("mem"), json).unwrap_err();
    assert!(!err.to_string().contains("hunter2"));
}

#[test]
fn connection_types_v1_fixture_loads() {
    let path = fixture("connection-types/v1.json");
    let bytes = std::fs::read(&path).unwrap();
    let file = document::decode::<ConnectionTypesFile>(&path, &bytes)
        .unwrap()
        .document;
    let types = ConnectionTypes::from_saved(file.types);
    assert_eq!(types.all().len(), 4);
    let staging = types
        .get(&ConnectionTypeId::new("staging").unwrap())
        .unwrap();
    assert!(staging.policy().confirm_destructive);
    assert!(types.resolve(&ConnectionTypeId::prod()).policy().production);
}
