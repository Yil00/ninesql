//! M1 (I1): a value rejected by any string newtype or enum of the workspace files never appears
//! in the resulting error message (which may be shown, logged or put in a crash report).
#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).

use std::path::Path;

use ninesql_core::{ConnectionTypesFile, ConnectionsFile, ProjectFile};
use ninesql_storage::document::{self, Document};
use serde_json::{Value, json};

/// Invalid for every newtype (upper case, `/`, space, `!`) and no enum variant.
const MARKER: &str = "Hunter2/Secret!";

fn connection() -> Value {
    json!({
        "id": "0b0c6c0e-6f0a-4d4e-9c47-1f1e0f3a9a01",
        "name": "n",
        "engine": "postgresql",
        "endpoint": { "kind": "tcp", "host": "h", "port": 5432 },
        "auth": { "method": "password", "username": "u",
                  "secret": { "source": "stored", "store": "keyring", "id": "x:auth" } },
        "ssh": { "host": "b", "username": "u",
                 "auth": { "method": "public-key", "key_path": "/k",
                           "passphrase": { "source": "prompt" } } },
        "tls": { "mode": "verify-full" },
        "connection_type": "dev",
        "origin": { "kind": "dbeaver", "dbeaver_id": "d" }
    })
}

fn assert_hidden<D: Document>(doc: Value, what: &str) {
    let bytes = serde_json::to_vec(&doc).unwrap();
    let err = match document::decode::<D>(Path::new("mem"), &bytes) {
        Ok(_) => panic!("{what}: the marker was accepted"),
        Err(e) => e,
    };
    let text = format!("{err} | {err:?}");
    assert!(!text.contains("Hunter2"), "{what}: {text}");
}

#[test]
fn rejected_values_are_never_echoed() {
    let fields = [
        "/id",
        "/engine",
        "/endpoint/kind",
        "/endpoint/port",
        "/auth/method",
        "/auth/secret/source",
        "/auth/secret/store",
        "/auth/secret/id",
        "/ssh/auth/method",
        "/ssh/port",
        "/tls/mode",
        "/connection_type",
        "/origin/kind",
        "/read_only",
        "/enabled",
        "/folder",
        "/properties",
    ];
    for pointer in fields {
        let mut c = connection();
        let (parent, key) = pointer.rsplit_once('/').unwrap();
        let target = if parent.is_empty() {
            &mut c
        } else {
            c.pointer_mut(parent).unwrap()
        };
        target[key] = Value::String(MARKER.to_owned());
        assert_hidden::<ConnectionsFile>(
            json!({ "schema_version": 1, "connections": [c] }),
            pointer,
        );
    }
}

#[test]
fn rejected_type_and_project_values_are_never_echoed() {
    let base = json!({
        "id": "staging", "name": "S", "color": "#FFFFFF", "auto_commit": true,
        "confirm_destructive": false, "confirm_every_write": false, "read_only": false,
        "log_query_text": true
    });
    for key in ["id", "color", "auto_commit"] {
        let mut t = base.clone();
        t[key] = Value::String(MARKER.to_owned());
        assert_hidden::<ConnectionTypesFile>(json!({ "schema_version": 1, "types": [t] }), key);
    }
    assert_hidden::<ProjectFile>(
        json!({ "schema_version": 1, "name": MARKER }),
        "project name",
    );
}

#[test]
fn unknown_field_names_are_kept_for_diagnosis() {
    // A key name, not a value, is shown when a secret reference has an extra field.
    let mut c = connection();
    c["auth"]["secret"]["extra"] = Value::String(MARKER.to_owned());
    let bytes = serde_json::to_vec(&json!({ "schema_version": 1, "connections": [c] })).unwrap();
    let err = document::decode::<ConnectionsFile>(Path::new("mem"), &bytes).unwrap_err();
    let text = err.to_string();
    assert!(text.contains("`extra`"), "{text}");
    assert!(!text.contains("Hunter2"), "{text}");
}
