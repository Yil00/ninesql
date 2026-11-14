//! Sanity checks on the synthetic DBeaver fixture workspaces (ROADMAP P0-E).
//!
//! These tests do not implement the importer (that is P2-A). They check that every
//! fixture has the expected layout, that `data-sources.json` is valid JSON, that
//! `credentials-config.json` is an obfuscated blob holding only the fake values listed
//! in `tests/fixtures/README.md`, and that the fixtures only point at fake hosts and paths.
//!
//! Assertion messages name fixture versions, files and connection ids, never credential
//! values (invariant I1 applies to test output too).

use std::collections::BTreeSet;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

use aes::cipher::{BlockDecryptMut, KeyIvInit, block_padding::Pkcs7};
use serde_json::Value;

type TestResult = Result<(), Box<dyn Error>>;

/// Minimum number of DBeaver versions covered (ROADMAP P0-E).
const MIN_VERSIONS: usize = 3;

/// Connection names created by `tests/fixtures/generate.sh` in every fixture.
const EXPECTED_CONNECTIONS: &[&str] = &[
    "pg-dev",
    "pg-prod-ssh",
    "pg-url-tls",
    "timescale-metrics",
    "mysql-test",
    "mariadb-ssh-key",
    "sqlite-local",
    "clickhouse-analytics",
    "oracle-legacy",
    "roundtrip-marker",
];

/// Fixed AES-128 key of DBeaver Community's `credentials-config.json` (public in DBeaver's
/// source code, documented in `tests/fixtures/README.md`).
const DBEAVER_CREDENTIALS_KEY: [u8; 16] = [
    0xba, 0xbb, 0x4a, 0x9f, 0x77, 0x4a, 0xb8, 0x53, 0xc9, 0x6c, 0x2d, 0x65, 0x3d, 0xfe, 0x54, 0x4a,
];

/// The only user names the fixtures may contain.
const FAKE_USERS: &[&str] = &["nine_user", "nine_ro", "nine_ssh"];

/// Files of a fixture, relative to `dbeaver-<version>/`.
const FIXTURE_FILES: &[&str] = &[
    "workspace6/.metadata/dbeaver-workspace.properties",
    "workspace6/General/.project",
    "workspace6/General/.dbeaver/data-sources.json",
    "workspace6/General/.dbeaver/credentials-config.json",
];

/// Files allowed directly under `tests/fixtures/`, besides the `dbeaver-<version>/` trees.
const TOP_LEVEL_FILES: &[&str] = &["README.md", "generate.sh", "hand-edit.py"];

type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;

fn fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

/// Returns `(version, workspace6 dir)` for every `dbeaver-<version>` fixture, sorted.
fn fixture_workspaces() -> Result<Vec<(String, PathBuf)>, Box<dyn Error>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(fixtures_root())? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some(version) = name.strip_prefix("dbeaver-") {
            if entry.file_type()?.is_dir() {
                out.push((version.to_owned(), entry.path().join("workspace6")));
            }
        }
    }
    out.sort();
    Ok(out)
}

fn data_sources_path(ws: &Path) -> PathBuf {
    ws.join("General")
        .join(".dbeaver")
        .join("data-sources.json")
}

fn credentials_path(ws: &Path) -> PathBuf {
    ws.join("General")
        .join(".dbeaver")
        .join("credentials-config.json")
}

fn read_json(path: &Path) -> Result<Value, Box<dyn Error>> {
    let text = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("{}: invalid JSON: {e}", path.display()).into())
}

/// Collects every string value stored under `key`, at any depth.
fn collect_strings(value: &Value, key: &str, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                if k == key {
                    if let Value::String(s) = v {
                        out.push(s.clone());
                    }
                }
                collect_strings(v, key, out);
            }
        }
        Value::Array(items) => items.iter().for_each(|v| collect_strings(v, key, out)),
        _ => {}
    }
}

/// Collects every string value, at any depth.
fn collect_all_strings(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(s) => out.push(s.clone()),
        Value::Object(map) => map.values().for_each(|v| collect_all_strings(v, out)),
        Value::Array(items) => items.iter().for_each(|v| collect_all_strings(v, out)),
        _ => {}
    }
}

/// Extracts the host of a `jdbc:<sub>://[user@]host[:port][/...]` URL.
fn jdbc_url_host(url: &str) -> Option<&str> {
    let rest = url.strip_prefix("jdbc:")?;
    let (_, after_scheme) = rest.split_once("://")?;
    let authority = after_scheme.split(['/', '?', ';']).next()?;
    let host_port = authority.rsplit('@').next()?;
    host_port.split(':').next()
}

fn assert_fake_host(version: &str, what: &str, host: &str) {
    assert!(
        host.ends_with(".invalid"),
        "[{version}] {what} {host:?} is not under .invalid"
    );
    assert!(
        !host.contains('@'),
        "[{version}] {what} {host:?} contains '@'"
    );
    assert!(
        !host.starts_with("10.") && !host.starts_with("192.168."),
        "[{version}] {what} {host:?} is a private IP address"
    );
}

/// Decrypts a DBeaver `credentials-config.json` (16-byte IV, AES-128-CBC, PKCS#7).
fn decrypt_credentials(bytes: &[u8]) -> Result<Value, Box<dyn Error>> {
    if bytes.len() <= 16 || bytes.len() % 16 != 0 {
        return Err(format!("unexpected size {}", bytes.len()).into());
    }
    let (iv, ciphertext) = bytes.split_at(16);
    let plaintext = Aes128CbcDec::new_from_slices(&DBEAVER_CREDENTIALS_KEY, iv)
        .map_err(|_| "bad key or IV length")?
        .decrypt_padded_vec_mut::<Pkcs7>(ciphertext)
        .map_err(|_| "bad padding (wrong key or corrupted file)")?;
    // Never include the plaintext in an error: it holds (fake) secrets.
    serde_json::from_slice(&plaintext).map_err(|_| "decrypted content is not JSON".into())
}

/// Lists every file under `dir`, relative to `root`, with `/` separators.
fn walk_files(root: &Path, dir: &Path, out: &mut BTreeSet<String>) -> TestResult {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            walk_files(root, &path, out)?;
        } else {
            let rel: Vec<String> = path
                .strip_prefix(root)?
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect();
            out.insert(rel.join("/"));
        }
    }
    Ok(())
}

#[test]
fn at_least_three_dbeaver_versions() -> TestResult {
    let versions: Vec<String> = fixture_workspaces()?.into_iter().map(|(v, _)| v).collect();
    assert!(
        versions.len() >= MIN_VERSIONS,
        "expected at least {MIN_VERSIONS} DBeaver versions, found {versions:?}"
    );
    Ok(())
}

#[test]
fn fixtures_directory_contains_only_allowed_files() -> TestResult {
    let root = fixtures_root();
    let mut actual = BTreeSet::new();
    walk_files(&root, &root, &mut actual)?;

    let mut expected: BTreeSet<String> = TOP_LEVEL_FILES.iter().map(|f| (*f).to_owned()).collect();
    for (version, _) in fixture_workspaces()? {
        for file in FIXTURE_FILES {
            expected.insert(format!("dbeaver-{version}/{file}"));
        }
    }
    let unexpected: Vec<&String> = actual.difference(&expected).collect();
    let missing: Vec<&String> = expected.difference(&actual).collect();
    assert!(
        unexpected.is_empty(),
        "unexpected files in tests/fixtures: {unexpected:?}"
    );
    assert!(
        missing.is_empty(),
        "missing files in tests/fixtures: {missing:?}"
    );
    Ok(())
}

#[test]
fn every_fixture_has_the_dbeaver_layout() -> TestResult {
    for (version, ws) in fixture_workspaces()? {
        let props = fs::read_to_string(ws.join(".metadata").join("dbeaver-workspace.properties"))?;
        assert!(
            props
                .lines()
                .any(|l| l.starts_with(&format!("product-version={version}."))),
            "[{version}] dbeaver-workspace.properties does not name version {version}"
        );

        let dot_project = fs::read_to_string(ws.join("General").join(".project"))?;
        assert!(
            dot_project.contains("org.jkiss.dbeaver.DBeaverNature"),
            "[{version}] .project lacks the DBeaver nature"
        );
    }
    Ok(())
}

#[test]
fn data_sources_are_valid_json_with_all_connections() -> TestResult {
    for (version, ws) in fixture_workspaces()? {
        let json = read_json(&data_sources_path(&ws))?;
        let connections = json
            .get("connections")
            .and_then(Value::as_object)
            .ok_or_else(|| format!("[{version}] no `connections` object"))?;
        let mut names: Vec<&str> = connections
            .values()
            .filter_map(|c| c.get("name").and_then(Value::as_str))
            .collect();
        names.sort_unstable();
        let mut expected = EXPECTED_CONNECTIONS.to_vec();
        expected.sort_unstable();
        assert_eq!(names, expected, "[{version}] connection names");
        assert!(
            json.get("folders").and_then(Value::as_object).is_some(),
            "[{version}] no `folders` object"
        );
        assert!(
            json.get("connection-types")
                .and_then(Value::as_object)
                .is_some(),
            "[{version}] no `connection-types` object"
        );
    }
    Ok(())
}

#[test]
fn credentials_are_obfuscated_and_hold_only_fake_values() -> TestResult {
    for (version, ws) in fixture_workspaces()? {
        let bytes = fs::read(credentials_path(&ws))?;
        assert!(
            serde_json::from_slice::<Value>(&bytes).is_err(),
            "[{version}] credentials-config.json must not be plaintext JSON"
        );
        assert!(
            !bytes.windows(b"fixture-".len()).any(|w| w == b"fixture-"),
            "[{version}] credentials-config.json contains a plaintext fixture value"
        );

        let creds = decrypt_credentials(&bytes)
            .map_err(|e| format!("[{version}] credentials-config.json: {e}"))?;
        let creds = creds
            .as_object()
            .ok_or_else(|| format!("[{version}] decrypted credentials are not an object"))?;
        let data_sources = read_json(&data_sources_path(&ws))?;
        let connection_ids: BTreeSet<&str> = data_sources
            .get("connections")
            .and_then(Value::as_object)
            .map(|m| m.keys().map(String::as_str).collect())
            .unwrap_or_default();

        let mut passwords = 0;
        for (id, sections) in creds {
            assert!(
                connection_ids.contains(id.as_str()),
                "[{version}] credentials for unknown connection id {id}"
            );
            let sections = sections
                .as_object()
                .ok_or_else(|| format!("[{version}] {id}: credentials are not an object"))?;
            for (section, values) in sections {
                assert!(
                    section == "#connection" || section == "network/ssh_tunnel",
                    "[{version}] {id}: unexpected credentials section {section}"
                );
                let values = values
                    .as_object()
                    .ok_or_else(|| format!("[{version}] {id} {section}: not an object"))?;
                for (key, value) in values {
                    let value = value
                        .as_str()
                        .ok_or_else(|| format!("[{version}] {id} {section} {key}: not a string"))?;
                    match key.as_str() {
                        "user" => assert!(
                            FAKE_USERS.contains(&value),
                            "[{version}] {id} {section}: user is not one of the fake users"
                        ),
                        "password" => {
                            passwords += 1;
                            assert!(
                                value.starts_with("fixture-"),
                                "[{version}] {id} {section}: password is not a fixture value"
                            );
                        }
                        other => panic!("[{version}] {id} {section}: unexpected key {other}"),
                    }
                }
            }
        }
        // 6 connection passwords + 2 SSH secrets (README.md, "Fake secret values").
        assert_eq!(passwords, 8, "[{version}] number of saved passwords");
    }
    Ok(())
}

#[test]
fn fixtures_only_reference_fake_hosts_and_paths() -> TestResult {
    for (version, ws) in fixture_workspaces()? {
        let fixture_dir = ws.parent().ok_or("fixture has no parent")?.to_path_buf();
        let mut files = BTreeSet::new();
        walk_files(&fixture_dir, &fixture_dir, &mut files)?;

        // Every text file of the fixture (all files except the obfuscated credentials).
        for rel in files
            .iter()
            .filter(|f| !f.ends_with("credentials-config.json"))
        {
            let text = fs::read_to_string(fixture_dir.join(rel))?;
            for forbidden in [
                "/root",
                "/tmp/",
                "localhost",
                "127.0.0.1",
                "192.168.",
                ".com",
                ".net",
                ".org",
            ] {
                assert!(
                    !text.contains(forbidden),
                    "[{version}] {rel} contains {forbidden:?}"
                );
            }
            for (pos, _) in text.match_indices("/home/") {
                assert!(
                    text[pos..].starts_with("/home/nine_user/"),
                    "[{version}] {rel} contains a /home/ path outside /home/nine_user/"
                );
            }
        }

        let json = read_json(&data_sources_path(&ws))?;
        let mut hosts = Vec::new();
        collect_strings(&json, "host", &mut hosts);
        assert!(!hosts.is_empty(), "[{version}] no host found");
        for host in &hosts {
            assert_fake_host(&version, "host", host);
        }

        let mut urls = Vec::new();
        collect_strings(&json, "url", &mut urls);
        assert!(!urls.is_empty(), "[{version}] no JDBC URL found");
        for url in &urls {
            let host = jdbc_url_host(url)
                .ok_or_else(|| format!("[{version}] cannot parse JDBC URL {url:?}"))?;
            assert_fake_host(&version, "JDBC URL host", host);
        }

        let mut strings = Vec::new();
        collect_all_strings(&json, &mut strings);
        for s in strings.iter().filter(|s| s.starts_with('/')) {
            assert!(
                s.starts_with("/home/nine_user/"),
                "[{version}] path {s:?} is outside /home/nine_user/"
            );
        }
    }
    Ok(())
}

#[test]
fn jdbc_url_host_extraction() {
    assert_eq!(
        jdbc_url_host("jdbc:postgresql://db.example.invalid:5433/nine?sslmode=require"),
        Some("db.example.invalid")
    );
    assert_eq!(
        jdbc_url_host("jdbc:mysql://user@db.example.invalid/nine"),
        Some("db.example.invalid")
    );
    assert_eq!(jdbc_url_host("jdbc:sqlite:/home/nine_user/x.sqlite"), None);
}
