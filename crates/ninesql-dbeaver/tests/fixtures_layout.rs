//! Sanity checks on the synthetic DBeaver fixture workspaces (ROADMAP P0-E).
//!
//! These tests do not parse the DBeaver model (that is P2-A); they only check that every
//! fixture has the expected layout, that `data-sources.json` is valid JSON, that
//! `credentials-config.json` is an encrypted blob (never plaintext JSON), and that the
//! fixtures only point at fake hosts. See `tests/fixtures/README.md`.

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

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
fn every_fixture_has_the_dbeaver_layout() -> TestResult {
    for (version, ws) in fixture_workspaces()? {
        let project = ws.join("General");
        for file in [
            project.join(".project"),
            project.join(".dbeaver").join("data-sources.json"),
            project.join(".dbeaver").join("credentials-config.json"),
            ws.join(".metadata").join("dbeaver-workspace.properties"),
        ] {
            assert!(file.is_file(), "[{version}] missing {}", file.display());
        }

        let props = fs::read_to_string(ws.join(".metadata").join("dbeaver-workspace.properties"))?;
        assert!(
            props
                .lines()
                .any(|l| l.starts_with(&format!("product-version={version}."))),
            "[{version}] dbeaver-workspace.properties does not name version {version}"
        );

        let dot_project = fs::read_to_string(project.join(".project"))?;
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
        let path = ws
            .join("General")
            .join(".dbeaver")
            .join("data-sources.json");
        let json = read_json(&path)?;
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
fn credentials_are_an_encrypted_blob_not_plaintext() -> TestResult {
    for (version, ws) in fixture_workspaces()? {
        let path = ws
            .join("General")
            .join(".dbeaver")
            .join("credentials-config.json");
        let bytes = fs::read(&path)?;
        // 16-byte IV followed by AES-CBC blocks (README.md documents the scheme).
        assert!(
            bytes.len() > 16 && bytes.len() % 16 == 0,
            "[{version}] unexpected credentials-config.json size {}",
            bytes.len()
        );
        assert!(
            serde_json::from_slice::<Value>(&bytes).is_err(),
            "[{version}] credentials-config.json must not be plaintext JSON"
        );
        assert!(
            !bytes.windows(b"fixture-".len()).any(|w| w == b"fixture-"),
            "[{version}] credentials-config.json contains a plaintext fixture value"
        );
    }
    Ok(())
}

#[test]
fn fixtures_only_reference_fake_hosts_and_paths() -> TestResult {
    for (version, ws) in fixture_workspaces()? {
        let path = ws
            .join("General")
            .join(".dbeaver")
            .join("data-sources.json");
        let text = fs::read_to_string(&path)?;
        for forbidden in ["/root", "/tmp/", "localhost", "127.0.0.1"] {
            assert!(
                !text.contains(forbidden),
                "[{version}] data-sources.json contains {forbidden:?}"
            );
        }
        let json: Value = serde_json::from_str(&text)?;
        let mut hosts = Vec::new();
        collect_strings(&json, "host", &mut hosts);
        assert!(!hosts.is_empty(), "[{version}] no host found");
        for host in hosts {
            assert!(
                host.ends_with(".invalid"),
                "[{version}] host {host:?} is not under .invalid"
            );
        }
        let mut urls = Vec::new();
        collect_strings(&json, "url", &mut urls);
        for url in urls {
            assert!(
                url.contains(".invalid"),
                "[{version}] url {url:?} is not under .invalid"
            );
        }
    }
    Ok(())
}
