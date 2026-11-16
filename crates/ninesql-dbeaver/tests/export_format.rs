//! P5-D: the exported `data-sources.json` uses only keys, nesting and value types that the
//! P0-E fixtures written by DBeaver CE 22.3.5 to 26.2.4 have in **every** version, except the
//! keys listed in [`SOURCE_ONLY`] (taken from DBeaver's source, no fixture: docs/06 marks them
//! *(source)* and the manual check protocol covers them).
// Test code: a failed unwrap is a failed test (AGENTS.md allows it outside library code).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use ninesql_dbeaver::export::{
    ExportAuth, ExportClientCertificate, ExportConnection, ExportConnectionType, ExportEndpoint,
    ExportHostKeyPolicy, ExportOptions, ExportSshAuth, ExportSshHop, ExportSshTunnel, ExportTls,
    Rgb, TlsMode,
};
use ninesql_dbeaver::export_connections;
use ninesql_driver_api::EngineId;
use serde_json::Value;

/// Key paths written by the export with no fixture covering them (generalized as in
/// [`shape`]). Each is documented in docs/06 "Export to DBeaver".
const SOURCE_ONLY: &[&str] = &[
    // The user names: DBeaver keeps them in credentials-config.json, which is never written.
    "connections.*.configuration.user",
    "connections.*.configuration.handlers.ssh_tunnel.user",
    "connections.*.configuration.bootstrap.defaultCatalog",
    "connections.*.configuration.handlers.mysql_ssl.properties.ssl.client.cert",
    "connections.*.configuration.handlers.mysql_ssl.properties.ssl.client.key",
    "connections.*.configuration.handlers.ssh_tunnel.properties.jumpServer.count",
    "connections.*.configuration.handlers.ssh_tunnel.properties.jumpServer<i>.enabled",
    "connections.*.configuration.handlers.ssh_tunnel.properties.jumpServer<i>.host",
    "connections.*.configuration.handlers.ssh_tunnel.properties.jumpServer<i>.port",
    "connections.*.configuration.handlers.ssh_tunnel.properties.jumpServer<i>.name",
    "connections.*.configuration.handlers.ssh_tunnel.properties.jumpServer<i>.authType",
    "connections.*.configuration.handlers.ssh_tunnel.properties.jumpServer<i>.keyPath",
];

/// JSON type name of a value.
fn kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Every key path of a data-sources document with its JSON type. User-chosen keys are
/// generalized: connection ids, folder and connection type names, and driver property names
/// become `*`; jump server indices become `<i>`.
fn shape(v: &Value) -> BTreeMap<String, BTreeSet<&'static str>> {
    let mut out: BTreeMap<String, BTreeSet<&'static str>> = BTreeMap::new();
    walk(v, "", &mut out);
    out
}

fn generalize(parent: &str, key: &str) -> String {
    let free = matches!(
        parent,
        "connections" | "folders" | "connection-types" | "connections.*.configuration.properties"
    );
    if free {
        return "*".to_owned();
    }
    if let Some(rest) = key.strip_prefix("jumpServer")
        && let Some((i, field)) = rest.split_once('.')
        && !i.is_empty()
        && i.bytes().all(|b| b.is_ascii_digit())
    {
        return format!("jumpServer<i>.{field}");
    }
    key.to_owned()
}

fn walk(v: &Value, path: &str, out: &mut BTreeMap<String, BTreeSet<&'static str>>) {
    if !path.is_empty() {
        out.entry(path.to_owned()).or_default().insert(kind(v));
    }
    if let Value::Object(map) = v {
        for (k, child) in map {
            let key = generalize(path, k);
            let p = if path.is_empty() {
                key
            } else {
                format!("{path}.{key}")
            };
            walk(child, &p, out);
        }
    }
}

/// Key paths present in every fixture version, with the union of their types.
fn fixture_shape() -> BTreeMap<String, BTreeSet<&'static str>> {
    let mut common_paths: Option<BTreeMap<String, BTreeSet<&'static str>>> = None;
    let workspaces = common::fixture_workspaces();
    assert_eq!(workspaces.len(), 5, "the five P0-E fixture versions");
    for (_, ws) in workspaces {
        let file = ws.join("General/.dbeaver/data-sources.json");
        let v: Value = serde_json::from_slice(&std::fs::read(file).unwrap()).unwrap();
        let s = shape(&v);
        common_paths = Some(match common_paths {
            None => s,
            Some(mut acc) => {
                acc.retain(|k, _| s.contains_key(k));
                for (k, kinds) in &mut acc {
                    kinds.extend(s[k].iter().copied());
                }
                acc
            }
        });
    }
    common_paths.unwrap()
}

/// Connections that make the export write every key it can.
fn everything() -> Vec<ExportConnection> {
    let hop = |host: &str, auth| ExportSshHop {
        host: host.to_owned(),
        port: 22,
        username: "nine_ssh".to_owned(),
        auth,
        host_key_policy: ExportHostKeyPolicy::Strict,
    };
    let key = || ExportSshAuth::PublicKey {
        key_path: PathBuf::from("/home/nine_user/.ssh/id_ed25519"),
        passphrase: Some(false),
    };
    let tls = |mode| ExportTls {
        mode,
        ca_file: Some(PathBuf::from("/home/nine_user/certs/ca.pem")),
        client_certificate: Some(ExportClientCertificate {
            cert_file: PathBuf::from("/home/nine_user/certs/client.pem"),
            key_file: PathBuf::from("/home/nine_user/certs/client.key"),
            has_passphrase: false,
        }),
    };
    let conn = |name: &str, engine: &str, mode| ExportConnection {
        id: name.to_owned(),
        name: name.to_owned(),
        description: Some("synthetic".to_owned()),
        engine: EngineId::new(engine.to_owned()),
        endpoint: ExportEndpoint::Tcp {
            host: "db.example.test".to_owned(),
            port: Some(5432),
            database: Some("nine".to_owned()),
        },
        auth: ExportAuth::Password {
            username: "nine_user".to_owned(),
            saved: false,
        },
        scram_only: false,
        ssh: Some(ExportSshTunnel {
            server: hop("bastion.example.test", key()),
            jump_hosts: vec![hop("jump.example.test", key())],
        }),
        tls: tls(mode),
        connection_type: "custom".to_owned(),
        read_only: Some(true),
        folder: vec!["A".to_owned(), "B".to_owned()],
        properties: BTreeMap::from([
            ("search_path".to_owned(), "app".to_owned()),
            ("work_mem".to_owned(), "64MB".to_owned()),
            ("time_zone".to_owned(), "UTC".to_owned()),
            ("busy_timeout".to_owned(), "100".to_owned()),
        ]),
        init_sql: vec!["SELECT 1".to_owned()],
        default_schema: Some("app".to_owned()),
        default_catalog: Some("nine".to_owned()),
        auto_commit: Some(false),
        disabled: None,
    };
    let mut sqlite = conn("sqlite", "sqlite", TlsMode::VerifyFull);
    sqlite.endpoint = ExportEndpoint::File {
        path: PathBuf::from("/srv/nine.sqlite"),
    };
    sqlite.ssh = None;
    vec![
        conn("pg", "postgresql", TlsMode::VerifyFull),
        conn("my-ca", "mysql", TlsMode::VerifyCa),
        conn("my-full", "mysql", TlsMode::VerifyFull),
        conn("maria-off", "mariadb", TlsMode::Disable),
        conn("maria-full", "mariadb", TlsMode::VerifyFull),
        conn("my-off", "mysql", TlsMode::Disable),
        sqlite,
    ]
}

fn custom_type() -> ExportConnectionType {
    ExportConnectionType {
        id: "custom".to_owned(),
        name: "Custom".to_owned(),
        color: Rgb { r: 1, g: 2, b: 3 },
        auto_commit: true,
        confirm_destructive: true,
        confirm_every_write: false,
        read_only: false,
        log_query_text: true,
        production: false,
    }
}

#[test]
fn exported_keys_and_types_match_every_fixture_version() {
    let export = export_connections(
        &everything(),
        &ExportOptions {
            connection_types: vec![custom_type()],
            timestamp_millis: Some(1),
        },
    )
    .unwrap();
    let out: Value = serde_json::from_slice(&export.json).unwrap();
    let fixtures = fixture_shape();
    let mut source_only_seen = BTreeSet::new();
    for (path, kinds) in shape(&out) {
        if SOURCE_ONLY.contains(&path.as_str()) {
            source_only_seen.insert(path);
            continue;
        }
        let Some(expected) = fixtures.get(&path) else {
            panic!("{path}: written by the export but not in every fixture version");
        };
        assert!(
            kinds.is_subset(expected),
            "{path}: type {kinds:?}, fixtures have {expected:?}"
        );
    }
    // The allowlist stays honest: every entry is actually written by this sample.
    let unused: Vec<&&str> = SOURCE_ONLY
        .iter()
        .filter(|p| !source_only_seen.contains(**p))
        .collect();
    assert!(unused.is_empty(), "unused SOURCE_ONLY entries: {unused:?}");
}

#[test]
fn layout_matches_dbeaver_style() {
    let options = ExportOptions {
        connection_types: vec![custom_type()],
        timestamp_millis: None,
    };
    let export = export_connections(&everything(), &options).unwrap();
    assert_eq!(export.report.exported_count(), everything().len());
    let text = String::from_utf8(export.json).unwrap();
    // Tab indentation and top-level key order as DBeaver writes them.
    assert!(text.starts_with("{\n\t\"folders\": {"));
    let pos = |k: &str| text.find(k).unwrap();
    assert!(pos("\"folders\"") < pos("\"connections\""));
    assert!(pos("\"connections\"") < pos("\"connection-types\""));
    // Nested folders in the 26.x full-path form, with every ancestor listed.
    let v: Value = serde_json::from_str(&text).unwrap();
    let folders: Vec<&String> = v["folders"].as_object().unwrap().keys().collect();
    assert_eq!(folders, ["A", "A/B"]);
    // Ports are strings, as DBeaver writes them.
    for c in v["connections"].as_object().unwrap().values() {
        if let Some(p) = c["configuration"].get("port") {
            assert!(p.is_string());
        }
    }
}
