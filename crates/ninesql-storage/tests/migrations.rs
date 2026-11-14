//! Migration engine tests against versioned fixtures (ADR-0008 §2).
#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use ninesql_storage::document::{self, backup_path};
use ninesql_storage::{Document, Format, Migration, StorageError};
use proptest::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Sample {
    items: Vec<Item>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Item {
    hostname: String,
    endpoint: Endpoint,
    #[serde(default)]
    tags: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Endpoint {
    port: u16,
}

fn items_mut(doc: &mut Value) -> Result<&mut Vec<Value>, String> {
    doc.get_mut("items")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| "`items` must be an array".to_owned())
}

fn v1_to_v2(doc: &mut Value) -> Result<(), String> {
    for item in items_mut(doc)? {
        let obj = item.as_object_mut().ok_or("item must be an object")?;
        if let Some(host) = obj.remove("host") {
            obj.insert("hostname".into(), host);
        }
    }
    Ok(())
}

fn v2_to_v3(doc: &mut Value) -> Result<(), String> {
    for item in items_mut(doc)? {
        let obj = item.as_object_mut().ok_or("item must be an object")?;
        let port = obj.remove("port").ok_or("item without `port`")?;
        obj.insert("endpoint".into(), serde_json::json!({ "port": port }));
        obj.entry("tags")
            .or_insert_with(|| Value::Array(Vec::new()));
    }
    Ok(())
}

const MIGRATIONS: &[Migration] = &[
    // Deliberately out of order: the engine looks steps up by `from`.
    Migration {
        from: 2,
        description: "move port under endpoint, add tags",
        apply: v2_to_v3,
    },
    Migration {
        from: 1,
        description: "rename host to hostname",
        apply: v1_to_v2,
    },
];

impl Document for Sample {
    const NAME: &'static str = "sample";
    const FORMAT: Format = Format::Json;
    const SCHEMA_VERSION: u32 = 3;
    fn migrations() -> &'static [Migration] {
        MIGRATIONS
    }
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct SampleToml {
    items: Vec<Item>,
}

impl Document for SampleToml {
    const NAME: &'static str = "sample-toml";
    const FORMAT: Format = Format::Toml;
    const SCHEMA_VERSION: u32 = 3;
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/sample")
        .join(name)
}

fn expected() -> Sample {
    Sample {
        items: vec![
            Item {
                hostname: "db.example.test".into(),
                endpoint: Endpoint { port: 5432 },
                tags: vec![],
            },
            Item {
                hostname: "localhost".into(),
                endpoint: Endpoint { port: 3306 },
                tags: vec![],
            },
        ],
    }
}

/// Copies a fixture into a private temp workspace.
fn workspace_copy(name: &str) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("sample.json");
    fs::copy(fixture(name), &path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    (tmp, path)
}

#[test]
fn every_fixture_version_decodes_to_the_current_model() {
    for (name, from) in [
        ("v1.json", Some(1)),
        ("v2.json", Some(2)),
        ("v3.json", None),
    ] {
        let bytes = fs::read(fixture(name)).unwrap();
        let loaded = document::decode::<Sample>(&fixture(name), &bytes).unwrap();
        assert_eq!(loaded.document, expected(), "{name}");
        assert_eq!(loaded.migrated_from, from, "{name}");
    }
}

#[test]
fn toml_fixture_decodes() {
    let bytes = fs::read(fixture("v3.toml")).unwrap();
    let loaded = document::decode::<SampleToml>(&fixture("v3.toml"), &bytes).unwrap();
    assert_eq!(loaded.document.items, expected().items);
    // And round-trips through the TOML encoder.
    let encoded = document::encode(&loaded.document).unwrap();
    let text = String::from_utf8(encoded.clone()).unwrap();
    assert!(text.starts_with("schema_version = 3"), "{text}");
    let again = document::decode::<SampleToml>(Path::new("mem"), &encoded).unwrap();
    assert_eq!(again.document, loaded.document);
}

#[test]
fn load_migrates_on_disk_and_keeps_a_backup() {
    let (_tmp, path) = workspace_copy("v1.json");
    let original = fs::read(&path).unwrap();
    let loaded = document::load::<Sample>(&path).unwrap().unwrap();
    assert_eq!(loaded.migrated_from, Some(1));
    assert_eq!(loaded.document, expected());

    // JSON backups are re-encoded from the scrubbed value (same content, canonical layout).
    let bak = backup_path(&path);
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(&bak).unwrap()).unwrap(),
        serde_json::from_slice::<Value>(&original).unwrap()
    );
    for p in [&path, &bak] {
        assert_eq!(fs::metadata(p).unwrap().permissions().mode() & 0o777, 0o600);
    }
    let rewritten: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(rewritten["schema_version"], 3);
    let text = fs::read_to_string(&path).unwrap();
    assert!(
        text.trim_start().starts_with("{\n  \"schema_version\": 3"),
        "{text}"
    );

    // A second load is a no-op.
    let again = document::load::<Sample>(&path).unwrap().unwrap();
    assert_eq!(again.migrated_from, None);
}

#[test]
fn newer_version_is_refused_and_left_untouched() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("sample.json");
    let content = br#"{ "schema_version": 4, "items": [], "new_field": true }"#;
    fs::write(&path, content).unwrap();
    let err = document::load::<Sample>(&path).unwrap_err();
    assert!(matches!(
        err,
        StorageError::NewerVersion {
            found: 4,
            supported: 3,
            ..
        }
    ));
    assert_eq!(fs::read(&path).unwrap(), content);
    assert!(!backup_path(&path).exists());
}

#[test]
fn missing_or_invalid_schema_version() {
    for content in [
        r#"{ "items": [] }"#,
        r#"{ "schema_version": "1", "items": [] }"#,
        r#"{ "schema_version": 0, "items": [] }"#,
        r#"{ "schema_version": -1, "items": [] }"#,
        r#"{ "schema_version": 4294967296, "items": [] }"#,
    ] {
        let err = document::decode::<Sample>(Path::new("mem"), content.as_bytes()).unwrap_err();
        assert!(
            matches!(err, StorageError::MissingSchemaVersion { .. }),
            "{content}: {err}"
        );
    }
}

#[test]
fn corrupt_file_is_left_untouched() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("sample.json");
    let content = b"{ \"schema_version\": 1, \"items\": [";
    fs::write(&path, content).unwrap();
    assert!(matches!(
        document::load::<Sample>(&path),
        Err(StorageError::Parse { .. })
    ));
    assert_eq!(fs::read(&path).unwrap(), content);
}

#[test]
fn failing_migration_leaves_the_file_untouched() {
    let (_tmp, path) = workspace_copy("v2.json");
    let broken = br#"{ "schema_version": 2, "items": [ { "hostname": "h" } ] }"#;
    fs::write(&path, broken).unwrap();
    let err = document::load::<Sample>(&path).unwrap_err();
    assert!(
        matches!(err, StorageError::Migration { from: 2, .. }),
        "{err}"
    );
    assert_eq!(fs::read(&path).unwrap(), broken);
    assert!(!backup_path(&path).exists());
}

#[test]
fn unknown_fields_are_tolerated_and_secret_like_keys_reported() {
    let content = br#"{
        "schema_version": 3,
        "future_top_level": { "x": 1 },
        "items": [ { "hostname": "h", "endpoint": { "port": 1 }, "password": "hunter2" } ]
    }"#;
    let loaded = document::decode::<Sample>(Path::new("mem"), content).unwrap();
    assert_eq!(loaded.document.items[0].hostname, "h");
    assert_eq!(loaded.secret_like_keys, vec!["items[0].password"]);
    // Dropped on save.
    let encoded = String::from_utf8(document::encode(&loaded.document).unwrap()).unwrap();
    assert!(!encoded.contains("hunter2"));
    assert!(!encoded.contains("future_top_level"));
}

#[test]
fn parse_errors_do_not_echo_values() {
    let content = br#"{ "schema_version": 3, "items": [ { "hostname": "h", "endpoint": { "port": "hunter2" } } ] }"#;
    let err = document::decode::<Sample>(Path::new("mem"), content).unwrap_err();
    let text = format!("{err} {err:?}");
    assert!(!text.contains("hunter2"), "{text}");
}

#[test]
fn load_of_missing_file_is_none() {
    let tmp = tempfile::tempdir().unwrap();
    assert!(
        document::load::<Sample>(&tmp.path().join("absent.json"))
            .unwrap()
            .is_none()
    );
}

fn item_strategy() -> impl Strategy<Value = Item> {
    (
        "\\PC{0,24}",
        any::<u16>(),
        proptest::collection::vec("\\PC{0,8}", 0..4),
    )
        .prop_map(|(hostname, port, tags)| Item {
            hostname,
            endpoint: Endpoint { port },
            tags,
        })
}

proptest! {
    #[test]
    fn json_encode_decode_round_trip(items in proptest::collection::vec(item_strategy(), 0..6)) {
        let doc = Sample { items };
        let bytes = document::encode(&doc).unwrap();
        let back = document::decode::<Sample>(Path::new("mem"), &bytes).unwrap();
        prop_assert_eq!(back.document, doc);
        prop_assert_eq!(back.migrated_from, None);
    }

    #[test]
    fn toml_encode_decode_round_trip(items in proptest::collection::vec(item_strategy(), 0..6)) {
        let doc = SampleToml { items };
        let bytes = document::encode(&doc).unwrap();
        let back = document::decode::<SampleToml>(Path::new("mem"), &bytes).unwrap();
        prop_assert_eq!(back.document, doc);
    }
}

#[test]
fn migration_backup_drops_secret_like_keys() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("sample.json");
    let content = br#"{ "schema_version": 1, "items": [
        { "host": "h", "port": 1, "password": "Hunter2-Secret", "note": "kept" } ] }"#;
    fs::write(&path, content).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let loaded = document::load::<Sample>(&path).unwrap().unwrap();
    assert_eq!(loaded.migrated_from, Some(1));
    assert_eq!(loaded.secret_like_keys, ["items[0].password"]);
    let bak = fs::read_to_string(backup_path(&path)).unwrap();
    assert!(!bak.contains("Hunter2-Secret"), "{bak}");
    assert!(bak.contains("\"note\": \"kept\""), "{bak}");
    assert!(bak.contains("\"schema_version\": 1"), "{bak}");
    assert!(
        !fs::read_to_string(&path)
            .unwrap()
            .contains("Hunter2-Secret")
    );
}

/// `Sample` with a `scrub` hook that removes tags starting with `pw:` (a stand-in for a model with
/// free-form values, like driver properties).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Scrubbed {
    items: Vec<Item>,
}

impl Document for Scrubbed {
    const NAME: &'static str = "scrubbed";
    const FORMAT: Format = Format::Json;
    const SCHEMA_VERSION: u32 = 3;
    fn migrations() -> &'static [Migration] {
        MIGRATIONS
    }
    fn scrub(value: &mut Value, schema_version: u32) {
        assert_eq!(
            schema_version, 1,
            "the hook sees the version of the original file"
        );
        for item in value["items"].as_array_mut().into_iter().flatten() {
            if let Some(tags) = item.get_mut("tags").and_then(Value::as_array_mut) {
                tags.retain(|t| !t.as_str().is_some_and(|t| t.starts_with("pw:")));
            }
        }
    }
}

#[test]
fn migration_backup_goes_through_the_scrub_hook() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("scrubbed.json");
    let content = br#"{ "schema_version": 1, "items": [
        { "host": "h", "port": 1, "tags": ["blue", "pw:Hunter2-Secret"] } ] }"#;
    fs::write(&path, content).unwrap();
    let loaded = document::load::<Scrubbed>(&path).unwrap().unwrap();
    assert_eq!(loaded.migrated_from, Some(1));
    // No secret-looking key: only the hook could catch this value.
    assert!(loaded.secret_like_keys.is_empty());
    let bak = fs::read_to_string(backup_path(&path)).unwrap();
    assert!(!bak.contains("Hunter2-Secret"), "{bak}");
    assert!(bak.contains("\"blue\""), "{bak}");
    assert!(bak.contains("\"schema_version\": 1"), "{bak}");
    assert_eq!(
        fs::metadata(backup_path(&path))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

/// Security review L2: duplicate JSON keys parse to their last value, so a clean-looking value
/// does not prove clean bytes. JSON backups are always re-encoded from the scrubbed value.
#[test]
fn json_backup_never_copies_a_duplicate_key_secret() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("scrubbed.json");
    let content = br#"{ "schema_version": 1, "items": [
        { "host": "h", "port": 1, "token": "Hunter2-Secret", "token": "" } ] }"#;
    fs::write(&path, content).unwrap();
    document::load::<Scrubbed>(&path).unwrap().unwrap();
    let bak = fs::read_to_string(backup_path(&path)).unwrap();
    assert!(!bak.contains("Hunter2-Secret"), "{bak}");
    assert!(bak.contains("\"schema_version\": 1"), "{bak}");
}

/// A TOML file (where duplicate keys are a parse error) with nothing to scrub is backed up
/// verbatim, comments included.
#[test]
fn clean_toml_backup_keeps_the_original_bytes() {
    #[derive(Debug, Serialize, Deserialize)]
    struct OldToml {
        items: Vec<Item>,
    }
    impl Document for OldToml {
        const NAME: &'static str = "old-toml";
        const FORMAT: Format = Format::Toml;
        const SCHEMA_VERSION: u32 = 3;
        fn migrations() -> &'static [Migration] {
            MIGRATIONS
        }
    }
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("settings.toml");
    let content =
        b"# hand-written comment\nschema_version = 1\n\n[[items]]\nhost = \"h\"\nport = 1\n";
    fs::write(&path, content).unwrap();
    let loaded = document::load::<OldToml>(&path).unwrap().unwrap();
    assert_eq!(loaded.migrated_from, Some(1));
    assert_eq!(fs::read(backup_path(&path)).unwrap(), content);
}

/// Security review L7 support: `scrub_file` cleans a file in place without the model.
#[test]
fn scrub_file_removes_secrets_and_keeps_the_rest() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("scrubbed.json");
    // `port` is invalid for the model; it must survive the scrub untouched.
    let content = br#"{ "schema_version": 3, "items": [
        { "hostname": "h", "endpoint": { "port": "not-a-port" }, "password": "Hunter2-Secret" } ] }"#;
    fs::write(&path, content).unwrap();
    assert!(document::scrub_file::<Sample>(&path).unwrap());
    let text = fs::read_to_string(&path).unwrap();
    assert!(!text.contains("Hunter2-Secret"), "{text}");
    assert!(text.contains("not-a-port"), "{text}");
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    // Clean now: no second rewrite.
    let ino = fs::metadata(&path).unwrap().ino();
    assert!(!document::scrub_file::<Sample>(&path).unwrap());
    assert_eq!(fs::metadata(&path).unwrap().ino(), ino);
    // Duplicate keys force a rewrite even when the parsed value looks clean.
    let dup = br#"{ "schema_version": 3, "token": "Hunter2-Secret", "token": "", "items": [] }"#;
    fs::write(&path, dup).unwrap();
    assert!(document::scrub_file::<Sample>(&path).unwrap());
    assert!(
        !fs::read_to_string(&path)
            .unwrap()
            .contains("Hunter2-Secret")
    );
    // Missing or unparsable files are left alone.
    assert!(!document::scrub_file::<Sample>(&tmp.path().join("absent.json")).unwrap());
    fs::write(&path, b"{ broken").unwrap();
    assert!(!document::scrub_file::<Sample>(&path).unwrap());
    assert_eq!(fs::read(&path).unwrap(), b"{ broken");
}

/// N7.2: a plaintext secret found on load is removed from disk at once, not at the next save.
#[test]
fn secret_like_keys_are_removed_from_disk_on_load() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("sample.json");
    let content = br#"{ "schema_version": 3,
        "items": [ { "hostname": "h", "endpoint": { "port": 1 }, "password": "Hunter2-Secret" } ] }"#;
    fs::write(&path, content).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    let loaded = document::load::<Sample>(&path).unwrap().unwrap();
    assert_eq!(loaded.secret_like_keys, ["items[0].password"]);
    assert_eq!(loaded.migrated_from, None);
    let text = fs::read_to_string(&path).unwrap();
    assert!(!text.contains("Hunter2-Secret"), "{text}");
    assert!(text.contains("\"hostname\": \"h\""), "{text}");
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    // No backup of a same-version file, and nothing else left in the directory.
    let names: Vec<_> = fs::read_dir(tmp.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, [std::ffi::OsString::from("sample.json")]);
}

/// A clean file is never rewritten on load.
#[test]
fn clean_file_is_not_rewritten_on_load() {
    let (_tmp, path) = workspace_copy("v3.json");
    let before = fs::metadata(&path).unwrap().ino();
    document::load::<Sample>(&path).unwrap().unwrap();
    assert_eq!(fs::metadata(&path).unwrap().ino(), before);
}
