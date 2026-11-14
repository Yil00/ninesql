//! I1: no secret, master password or label reaches `vault.bin`, any other file of the directory,
//! the logs or an error message.
//!
//! Its own test binary with a single test: the log capture is the **global** tracing subscriber,
//! installed before any callsite is hit, and it answers `Interest::always()`. A per-thread
//! subscriber (`with_default`) in the shared unit-test binary missed events now and then, because
//! tracing caches callsite interest process-wide while other tests run in parallel.
#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).

use std::fmt;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use ninesql_secrets::{
    ExposeSecret, KdfParams, SecretId, SecretKind, SecretMeta, SecretStore, SecretString, Vault,
    VaultStore,
};
use tracing::level_filters::LevelFilter;
use tracing::subscriber::Interest;

const PROJECT_A: &str = "3f2a6c1e-0b7d-4c51-9e2a-6d8f0a1b2c3d";
const MASTER: &str = "correct horse battery staple";
const NEW_MASTER: &str = "tr0ub4dor&3-but-longer";
const SECRET: &str = "hunter2-Sup3rS3cret!";
const LABEL: &str = "Accounting replica label";
const CONN: &str = "8a6e0f62-6b0c-4d8e-9f1e-2a4b6c8d0e1f";

fn s(v: &str) -> SecretString {
    SecretString::from(v.to_owned())
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Records every span and event field.
struct Capture(Arc<Mutex<String>>);

struct Fields<'a>(&'a mut String);

impl tracing::field::Visit for Fields<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn fmt::Debug) {
        use std::fmt::Write;
        let _ = write!(self.0, " {}={:?}", field.name(), value);
    }
}

impl tracing::Subscriber for Capture {
    fn register_callsite(&self, _: &'static tracing::Metadata<'static>) -> Interest {
        Interest::always()
    }
    fn max_level_hint(&self) -> Option<LevelFilter> {
        Some(LevelFilter::TRACE)
    }
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, attrs: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        attrs.record(&mut Fields(&mut self.0.lock().unwrap()));
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, values: &tracing::span::Record<'_>) {
        values.record(&mut Fields(&mut self.0.lock().unwrap()));
    }
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        let mut out = self.0.lock().unwrap();
        out.push_str(event.metadata().target());
        event.record(&mut Fields(&mut out));
        out.push('\n');
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

#[test]
fn no_plaintext_in_the_file_logs_or_errors() {
    let logs = Arc::new(Mutex::new(String::new()));
    tracing::subscriber::set_global_default(Capture(logs.clone())).unwrap();
    tracing::callsite::rebuild_interest_cache();

    let dir = tempfile::tempdir().unwrap();
    let path: PathBuf = dir.path().join("vault.bin");
    let bytes = || fs::read(&path).unwrap();
    let vault = Arc::new(Vault::open_at(&path, &dir.path().join("ninesql.lock")).unwrap());
    // Smallest parameters a real vault accepts (19 MiB, t = 2): `insecure_for_tests` is not
    // available to integration tests without the `memory-store` feature.
    let params = KdfParams::new(KdfParams::MIN_M_COST_KIB, KdfParams::MIN_T_COST, 1).unwrap();
    let auth = SecretId::new(format!("{CONN}:auth")).unwrap();
    let meta = SecretMeta {
        connection_id: CONN.into(),
        kind: SecretKind::Password,
        label: LABEL.into(),
    };
    let mut errors = String::new();

    vault.create(&s(MASTER), params).unwrap();
    let a = VaultStore::new(vault.clone(), PROJECT_A).unwrap();
    a.set(&auth, &meta, &s(SECRET)).unwrap();
    vault.lock();
    errors.push_str(&vault.unlock(&s(NEW_MASTER)).unwrap_err().to_string());
    vault.unlock(&s(MASTER)).unwrap();
    assert_eq!(a.get(&auth).unwrap().expose_secret(), SECRET);
    errors.push_str(
        &vault
            .change_master_password(&s(SECRET), &s(NEW_MASTER), None)
            .unwrap_err()
            .to_string(),
    );
    vault
        .change_master_password(&s(MASTER), &s(NEW_MASTER), None)
        .unwrap();
    let mut corrupt = bytes();
    let last = corrupt.len() - 1;
    corrupt[last] ^= 1;
    let good = bytes();
    fs::write(&path, &corrupt).unwrap();
    vault.lock();
    errors.push_str(&vault.unlock(&s(NEW_MASTER)).unwrap_err().to_string());
    fs::write(&path, &good).unwrap();
    vault.unlock(&s(NEW_MASTER)).unwrap();
    errors.push_str(&format!("{vault:?} {a:?}"));
    // Through the SecretStore error mapping as well.
    vault.lock();
    errors.push_str(&a.get(&auth).unwrap_err().to_string());

    let logs = logs.lock().unwrap().clone();
    // The capture saw the whole flow, not just part of it.
    for expected in [
        "vault created",
        "vault written",
        "vault unlocked",
        "vault unlock failed",
        "vault master password changed",
        "vault locked",
    ] {
        assert!(logs.contains(expected), "{expected:?} not captured: {logs}");
    }
    for needle in [MASTER, NEW_MASTER, SECRET, LABEL] {
        assert!(!logs.contains(needle), "{needle:?} in logs");
        assert!(!errors.contains(needle), "{needle:?} in errors");
        for entry in fs::read_dir(dir.path()).unwrap() {
            let file = entry.unwrap().path();
            let content = fs::read(&file).unwrap();
            assert!(
                !contains(&content, needle.as_bytes()),
                "{needle:?} in {}",
                file.display()
            );
        }
    }
    // The connection id and secret id are encrypted too.
    assert!(!contains(&bytes(), CONN.as_bytes()));
    // No temp file left next to the vault.
    let mut names: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, ["ninesql.lock", "vault.bin"]);
}
