//! What a run prepares before the window opens (ADR-0029 §2.5) and checks after it (§2.10):
//! the dev engines of `dev/engines.json`, the files of the run's HOME (SQLite "nine" files,
//! copies of synthetic DBeaver fixtures, small text files), the connections the runner saves
//! (secrets in the run's vault), the random master password and the I1 scan.

use std::path::{Path, PathBuf};
use std::time::Duration;

use ninesql_core::secrets::SecretString;
use ninesql_core::{
    Auth, Connection, ConnectionTypeId, Endpoint, EngineId, SecretSlot, SecretSource, TlsMode,
};
use zeroize::Zeroizing;

use super::scenario::{ConnectionKind, SQLITE, Scenario, ScenarioConnection};
use crate::connections::form::BuiltConnection;

/// A dev engine of `dev/engines.json`. Its password is a throwaway dev credential.
#[derive(Clone)]
pub struct DevEngine {
    /// Engine id (`postgresql`…).
    pub id: String,
    /// Host (loopback).
    pub host: String,
    /// Port.
    pub port: u16,
    /// User.
    pub user: String,
    /// Password (dev only).
    pub password: String,
    /// Database.
    pub database: String,
}

impl std::fmt::Debug for DevEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DevEngine")
            .field("id", &self.id)
            .field("host", &self.host)
            .field("port", &self.port)
            .finish_non_exhaustive()
    }
}

/// `dev/engines.json`: its engines and every password it lists.
#[derive(Debug, Clone)]
pub struct DevEngines {
    engines: Vec<DevEngine>,
    passwords: Vec<String>,
}

/// Largest `engines.json` read.
const MAX_ENGINES_BYTES: u64 = 1024 * 1024;

impl DevEngines {
    /// Reads `path`.
    ///
    /// # Errors
    /// A message without any value of the file.
    pub fn load(path: &Path) -> Result<Self, String> {
        let bytes = ninesql_core::storage::fs::read_limited(path, MAX_ENGINES_BYTES)
            .map_err(|_| "dev/engines.json cannot be read".to_owned())?
            .ok_or("dev/engines.json does not exist")?;
        let doc: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|_| "dev/engines.json is not valid JSON")?;
        Ok(Self::from_json(&doc))
    }

    /// From a parsed `engines.json`.
    #[must_use]
    pub fn from_json(doc: &serde_json::Value) -> Self {
        let engines = doc["engines"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|e| {
                        let text = |k: &str| e[k].as_str().unwrap_or_default().to_owned();
                        Some(DevEngine {
                            id: text("id"),
                            host: text("host"),
                            port: e["port"].as_u64().and_then(|p| u16::try_from(p).ok())?,
                            user: text("user"),
                            password: text("password"),
                            database: text("database"),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self {
            engines,
            passwords: passwords_in(doc),
        }
    }

    /// The engine `id`.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&DevEngine> {
        self.engines.iter().find(|e| e.id == id)
    }

    /// Every password of the file (its `"password"` fields and the `password: <value>` of its
    /// notes), at least 4 characters, longest first: what the I1 scan looks for.
    #[must_use]
    pub fn passwords(&self) -> &[String] {
        &self.passwords
    }
}

/// The distinct passwords of a parsed `engines.json`, longest first.
fn passwords_in(doc: &serde_json::Value) -> Vec<String> {
    fn note_passwords(text: &str, found: &mut std::collections::BTreeSet<String>) {
        const KEY: &str = "password:";
        let mut rest = text;
        while let Some(at) = rest.find(KEY) {
            rest = rest[at + KEY.len()..].trim_start();
            let end = rest
                .find(|c: char| c.is_whitespace() || c == ',' || c == ';')
                .unwrap_or(rest.len());
            let value = rest[..end].trim_end_matches('.');
            if !value.is_empty() {
                found.insert(value.to_owned());
            }
            rest = &rest[end..];
        }
    }
    fn walk(v: &serde_json::Value, found: &mut std::collections::BTreeSet<String>) {
        match v {
            serde_json::Value::Object(map) => {
                for (k, x) in map {
                    if k == "password"
                        && let Some(p) = x.as_str()
                        && !p.is_empty()
                    {
                        found.insert(p.to_owned());
                    }
                    walk(x, found);
                }
            }
            serde_json::Value::Array(items) => items.iter().for_each(|x| walk(x, found)),
            serde_json::Value::String(s) => note_passwords(s, found),
            _ => {}
        }
    }
    let mut found = std::collections::BTreeSet::new();
    walk(doc, &mut found);
    let mut out: Vec<String> = found.into_iter().filter(|w| w.len() >= 4).collect();
    out.sort_by_key(|w| std::cmp::Reverse(w.len()));
    out
}

/// Whether the dev engine answers on TCP (2 s). **Blocking.**
#[must_use]
pub fn reachable(engine: &DevEngine) -> bool {
    format!("{}:{}", engine.host, engine.port)
        .parse::<std::net::SocketAddr>()
        .ok()
        .is_some_and(|a| std::net::TcpStream::connect_timeout(&a, Duration::from_secs(2)).is_ok())
}

/// A random master password for the run's vault, never written anywhere (ADR-0029 §2.5).
#[must_use]
pub fn master_password() -> Zeroizing<String> {
    Zeroizing::new(format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    ))
}

/// Writes the scenario's files in the run's `home`: SQLite "nine" files from
/// `dev/seed/sqlite/nine.sql` (`big_events` cut to 1 000 rows), copies of the DBeaver fixtures
/// at `~/.local/share/DBeaverData/workspace6` (where "Import from DBeaver" looks), and the
/// `[[file]]` entries. **Blocking.**
///
/// # Errors
/// A message naming what could not be written.
pub fn prepare_home(home: &Path, scenario: &Scenario, repo_root: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let seed_path = repo_root.join("dev/seed/sqlite/nine.sql");
    for c in &scenario.connections {
        if let Some(file) = &c.file {
            let seed = std::fs::read_to_string(&seed_path)
                .map_err(|_| "dev/seed/sqlite/nine.sql cannot be read".to_owned())?;
            let seed = seed.replace("i < 1000000", "i < 1000");
            let path = home.join(file);
            let db = rusqlite::Connection::open(&path)
                .map_err(|_| "the SQLite file of a scenario cannot be created".to_owned())?;
            db.execute_batch(&seed)
                .map_err(|_| "the SQLite seed did not run".to_owned())?;
            drop(db);
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
    }
    for fixture in &scenario.dbeaver {
        let target = home.join(".local/share/DBeaverData/workspace6");
        copy_tree(&fixture.join("workspace6"), &target)
            .map_err(|_| "a DBeaver fixture could not be copied".to_owned())?;
    }
    for f in &scenario.files {
        std::fs::write(home.join(&f.name), f.content.as_bytes())
            .map_err(|_| "a scenario file could not be written".to_owned())?;
    }
    Ok(())
}

/// Copies regular files and directories; symbolic links are skipped, never followed.
fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let dest = to.join(entry.file_name());
        if ty.is_dir() {
            copy_tree(&entry.path(), &dest)?;
        } else if ty.is_file() {
            std::fs::copy(entry.path(), &dest)?;
        }
    }
    Ok(())
}

/// The connection the runner saves for `c`, with its password to store in the run's vault.
/// Dev engines connect over loopback in plaintext: their TLS uses a throwaway CA, and
/// plaintext is the visible, per-connection choice (I5).
///
/// # Errors
/// The engine is not in `engines.json`.
pub fn built_connection(
    c: &ScenarioConnection,
    engines: Option<&DevEngines>,
    home: &Path,
) -> Result<BuiltConnection, String> {
    let ty = match c.kind {
        ConnectionKind::Dev => ConnectionTypeId::dev(),
        ConnectionKind::Test => ConnectionTypeId::test(),
        ConnectionKind::Production => ConnectionTypeId::prod(),
    };
    let engine = EngineId::new(c.engine.as_str()).map_err(|e| e.to_string())?;
    let (endpoint, auth, to_store) = if c.engine == SQLITE {
        let file = home.join(c.file.as_deref().unwrap_or("nine.db"));
        (Endpoint::File { path: file }, Auth::None, Vec::new())
    } else {
        let dev = engines
            .and_then(|e| e.get(&c.engine))
            .ok_or_else(|| format!("{} is not in dev/engines.json", c.engine))?;
        (
            Endpoint::Tcp {
                host: dev.host.clone(),
                port: Some(dev.port),
                database: Some(dev.database.clone()),
            },
            Auth::Password {
                username: dev.user.clone(),
                secret: SecretSource::Prompt,
            },
            vec![(SecretSlot::Auth, SecretString::from(dev.password.clone()))],
        )
    };
    let mut connection = Connection::new(c.name.clone(), engine, endpoint, auth, ty);
    connection.read_only = Some(c.read_only);
    if c.engine != SQLITE {
        connection.tls.mode = TlsMode::Disable;
    }
    Ok(BuiltConnection {
        connection,
        to_store,
        to_delete: Vec::new(),
    })
}

/// I1 (ADR-0029 §2.10): the files under `run_dir` (the vault excepted, ciphertext) that hold
/// one of `secrets`, as paths relative to `run_dir`. Never returns a value. **Blocking.**
#[must_use]
pub fn scan_for_secrets(
    run_dir: &Path,
    secrets: &[&str],
    exclude: &[PathBuf],
) -> (usize, Vec<String>) {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let Ok(ty) = e.file_type() else { continue };
                if ty.is_dir() {
                    walk(&e.path(), out);
                } else if ty.is_file() {
                    out.push(e.path());
                }
            }
        }
    }
    let mut files = Vec::new();
    walk(run_dir, &mut files);
    let mut hits = Vec::new();
    let mut scanned = 0;
    for f in files {
        if exclude.iter().any(|x| x == &f) {
            continue;
        }
        scanned += 1;
        let Ok(bytes) = std::fs::read(&f) else {
            continue;
        };
        if secrets
            .iter()
            .filter(|s| !s.is_empty())
            .any(|s| bytes.windows(s.len()).any(|w| w == s.as_bytes()))
        {
            hits.push(
                f.strip_prefix(run_dir)
                    .unwrap_or(&f)
                    .to_string_lossy()
                    .into_owned(),
            );
        }
    }
    (scanned, hits)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).
mod tests {
    use super::*;

    #[test]
    fn passwords_of_fields_and_notes() {
        let doc = serde_json::json!({
            "engines": [
                { "id": "a", "host": "127.0.0.1", "port": 1, "password": "first_pw" },
                { "id": "b", "port": 2, "password": "first_pw", "notes": "root password: second_pw (x)" },
                { "id": "c", "port": 3, "password": "abc" }
            ]
        });
        let e = DevEngines::from_json(&doc);
        assert_eq!(e.passwords(), ["second_pw", "first_pw"]);
        assert_eq!(e.get("a").unwrap().port, 1);
        assert!(!format!("{:?}", e.get("a").unwrap()).contains("first_pw"));
    }

    #[test]
    fn the_scan_finds_secrets_but_skips_the_vault() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("data/ninesql")).unwrap();
        std::fs::write(root.join("data/ninesql/connections.json"), b"{}").unwrap();
        std::fs::write(root.join("data/ninesql/vault.bin"), b"..s3cret-value..").unwrap();
        std::fs::write(root.join("leak.log"), b"x s3cret-value y").unwrap();
        let (scanned, hits) = scan_for_secrets(
            root,
            &["s3cret-value"],
            &[root.join("data/ninesql/vault.bin")],
        );
        assert_eq!(scanned, 2);
        assert_eq!(hits, ["leak.log"]);
    }

    #[test]
    fn master_passwords_are_long_and_fresh() {
        let a = master_password();
        let b = master_password();
        assert_eq!(a.len(), 64);
        assert_ne!(*a, *b);
    }
}
