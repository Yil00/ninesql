//! On-disk format of `connections.json` and `connection-types.json`, locked by versioned fixtures.
#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).

use std::path::{Path, PathBuf};

use ninesql_core::{
    Auth, ConnectionTypeId, ConnectionTypes, ConnectionTypesFile, ConnectionsFile, Endpoint,
    HostKeyPolicy, Origin, PasswordExchange, SecretSource, SecurityWarning, SshAuth, TlsMode,
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
    // Version 1 → 3 changes nothing but the version: a hop without `host_key_policy` is strict,
    // a connection without `password_exchange` is `any`.
    let path = fixture("connections/v1.json");
    let mut original: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    original["schema_version"] = Value::from(3);
    let file = decode_connections("connections/v1.json");
    let encoded: Value = serde_json::from_slice(&document::encode(&file).unwrap()).unwrap();
    assert_eq!(encoded, original);
    let ssh = file.connections[0].ssh.as_ref().unwrap();
    assert!(
        ssh.hops()
            .all(|h| h.host_key_policy == HostKeyPolicy::Strict)
    );
}

#[test]
fn connections_v2_fixture_keeps_per_hop_host_key_policies() {
    let path = fixture("connections/v2.json");
    let mut original: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    // Version 2 → 3 changes nothing but the version.
    original["schema_version"] = Value::from(3);
    let file = decode_connections("connections/v2.json");
    let c = &file.connections[0];
    c.validate().unwrap();
    let ssh = c.ssh.as_ref().unwrap();
    let policies: Vec<_> = ssh.hops().map(|h| h.host_key_policy).collect();
    assert_eq!(
        policies,
        [
            HostKeyPolicy::AcceptNew,
            HostKeyPolicy::Disabled,
            HostKeyPolicy::Strict
        ]
    );
    assert_eq!(
        c.security_warnings(),
        [
            SecurityWarning::SshAcceptNewHostKeys { hop: 0 },
            SecurityWarning::SshHostKeyNotVerified { hop: 1 }
        ]
    );
    let encoded: Value = serde_json::from_slice(&document::encode(&file).unwrap()).unwrap();
    assert_eq!(encoded, original, "strict is not written, downgrades are");
}

#[test]
fn connections_v3_fixture_keeps_the_password_exchange_policy() {
    let path = fixture("connections/v3.json");
    let original: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let file = decode_connections("connections/v3.json");
    let (scram, any) = (&file.connections[0], &file.connections[1]);
    scram.validate().unwrap();
    assert_eq!(scram.password_exchange, PasswordExchange::ScramOnly);
    assert_eq!(any.password_exchange, PasswordExchange::Any);
    // SCRAM below verify-ca is flagged (no channel binding), verify-full is not.
    assert!(scram.scram_without_verified_tls());
    assert!(!any.scram_without_verified_tls());
    let encoded: Value = serde_json::from_slice(&document::encode(&file).unwrap()).unwrap();
    assert_eq!(encoded, original, "`any` is not written, `scram-only` is");
}

#[test]
fn a_password_exchange_in_a_v2_file_is_ignored() {
    // Only a NineSQL that knows (and validates) the field writes it.
    let json = br#"{ "schema_version": 2, "connections": [ {
        "id": "0b0c6c0e-6f0a-4d4e-9c47-1f1e0f3a9a99", "name": "n", "engine": "postgresql",
        "endpoint": { "kind": "tcp", "host": "h" }, "auth": { "method": "none" },
        "connection_type": "dev", "password_exchange": "scram-only"
    } ] }"#;
    let loaded = document::decode::<ConnectionsFile>(Path::new("mem"), json).unwrap();
    assert_eq!(loaded.migrated_from, Some(2));
    assert_eq!(
        loaded.document.connections[0].password_exchange,
        PasswordExchange::Any
    );
}

#[test]
fn a_newer_connections_file_is_refused_and_left_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("connections.json");
    let bytes = br#"{ "schema_version": 4, "connections": [] }"#;
    std::fs::write(&path, bytes).unwrap();
    assert!(document::load::<ConnectionsFile>(&path).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
}

#[test]
fn a_v2_file_on_disk_is_migrated_with_a_backup() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("connections.json");
    let v2 = std::fs::read(fixture("connections/v2.json")).unwrap();
    std::fs::write(&path, &v2).unwrap();
    let loaded = document::load::<ConnectionsFile>(&path).unwrap().unwrap();
    assert_eq!(loaded.migrated_from, Some(2));
    let backup: Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("connections.json.bak")).unwrap())
            .unwrap();
    assert_eq!(backup["schema_version"], 2);
    let migrated: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(migrated["schema_version"], 3);
}

#[test]
fn a_host_key_policy_in_a_v1_file_is_ignored() {
    // Only a NineSQL that knows the field writes a downgrade; a hand-edited v1 file stays strict.
    let json = br#"{ "schema_version": 1, "connections": [ {
        "id": "0b0c6c0e-6f0a-4d4e-9c47-1f1e0f3a9a99", "name": "n", "engine": "postgresql",
        "endpoint": { "kind": "tcp", "host": "h" }, "auth": { "method": "none" },
        "connection_type": "dev",
        "ssh": { "host": "b", "username": "u", "auth": { "method": "agent" },
                 "host_key_policy": "disabled",
                 "jump_hosts": [ { "host": "j", "username": "u", "auth": { "method": "agent" },
                                   "host_key_policy": "accept-new" } ] }
    } ] }"#;
    let file = document::decode::<ConnectionsFile>(Path::new("mem"), json)
        .unwrap()
        .document;
    let ssh = file.connections[0].ssh.as_ref().unwrap();
    assert!(
        ssh.hops()
            .all(|h| h.host_key_policy == HostKeyPolicy::Strict)
    );
    assert!(file.connections[0].security_warnings().is_empty());
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
