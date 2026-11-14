//! Vault tests: round trips, wrong password, tampering, truncation, KDF parameters, password
//! change, locking, back-off, permissions and the absence of plaintext in the file and logs (I1).
#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).

use std::fs::{self, File};
use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use proptest::prelude::*;
use tempfile::TempDir;

use super::*;

const PROJECT_A: &str = "3f2a6c1e-0b7d-4c51-9e2a-6d8f0a1b2c3d";
const PROJECT_B: &str = "00000000-0000-4000-8000-000000000000";
const MASTER: &str = "correct horse battery staple";
const NEW_MASTER: &str = "tr0ub4dor&3-but-longer";
const SECRET: &str = "hunter2-Sup3rS3cret!";
const LABEL: &str = "Accounting replica label";
const CONN: &str = "8a6e0f62-6b0c-4d8e-9f1e-2a4b6c8d0e1f";

fn s(v: &str) -> SecretString {
    SecretString::from(v.to_owned())
}

fn id(v: &str) -> SecretId {
    SecretId::new(v).unwrap()
}

fn auth_id() -> SecretId {
    id(&format!("{CONN}:auth"))
}

fn meta() -> SecretMeta {
    SecretMeta {
        connection_id: CONN.into(),
        kind: SecretKind::Password,
        label: LABEL.into(),
    }
}

struct Fixture {
    dir: TempDir,
    vault: Arc<Vault>,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let vault = Arc::new(open(&dir));
        Self { dir, vault }
    }

    fn created() -> Self {
        let f = Self::new();
        f.vault
            .create(&s(MASTER), KdfParams::insecure_for_tests())
            .unwrap();
        f
    }

    fn path(&self) -> PathBuf {
        self.dir.path().join("vault.bin")
    }

    fn bytes(&self) -> Vec<u8> {
        fs::read(self.path()).unwrap()
    }

    fn store(&self, project: &str) -> VaultStore {
        VaultStore::new(self.vault.clone(), project).unwrap()
    }
}

fn open(dir: &TempDir) -> Vault {
    Vault::open_at(
        dir.path().join("vault.bin"),
        &dir.path().join("ninesql.lock"),
    )
    .unwrap()
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[test]
fn round_trip_across_reopen() {
    let f = Fixture::created();
    let a = f.store(PROJECT_A);
    a.set(&auth_id(), &meta(), &s(SECRET)).unwrap();
    let ssh = id(&format!("{CONN}:ssh"));
    a.set(
        &ssh,
        &SecretMeta {
            kind: SecretKind::SshKeyPassphrase,
            ..meta()
        },
        &s("ünïcødé passphrase"),
    )
    .unwrap();
    assert_eq!(a.get(&auth_id()).unwrap().expose_secret(), SECRET);

    // A new process: new vault instance, locked at start.
    let dir = f.dir;
    drop(a);
    drop(f.vault);
    let vault = Arc::new(open(&dir));
    assert_eq!(vault.status().unwrap(), VaultStatus::Locked);
    let a = VaultStore::new(vault.clone(), PROJECT_A).unwrap();
    assert!(matches!(
        a.get(&auth_id()),
        Err(SecretError::Locked(StoreKind::Vault))
    ));
    vault.unlock(&s(MASTER)).unwrap();
    assert_eq!(a.get(&auth_id()).unwrap().expose_secret(), SECRET);
    assert_eq!(a.get(&ssh).unwrap().expose_secret(), "ünïcødé passphrase");
    assert_eq!(a.ids().unwrap(), vec![auth_id(), ssh.clone()]);
    // Replace, delete.
    a.set(&auth_id(), &meta(), &s("second")).unwrap();
    assert_eq!(a.get(&auth_id()).unwrap().expose_secret(), "second");
    assert!(a.delete(&ssh).unwrap());
    assert!(!a.delete(&ssh).unwrap());
    assert!(matches!(a.get(&ssh), Err(SecretError::NotFound(_))));
    vault.lock();
    vault.unlock(&s(MASTER)).unwrap();
    assert_eq!(a.ids().unwrap(), vec![auth_id()]);
}

#[test]
fn projects_are_isolated() {
    let f = Fixture::created();
    let a = f.store(PROJECT_A);
    let b = f.store(&PROJECT_B.to_ascii_uppercase());
    a.set(&auth_id(), &meta(), &s("for-a")).unwrap();
    assert!(matches!(b.get(&auth_id()), Err(SecretError::NotFound(_))));
    b.set(&auth_id(), &meta(), &s("for-b")).unwrap();
    assert_eq!(a.get(&auth_id()).unwrap().expose_secret(), "for-a");
    assert_eq!(b.get(&auth_id()).unwrap().expose_secret(), "for-b");
    assert!(b.delete(&auth_id()).unwrap());
    assert_eq!(a.get(&auth_id()).unwrap().expose_secret(), "for-a");
    assert!(VaultStore::new(f.vault.clone(), "General").is_err());
}

#[test]
fn not_created_and_locked_mapping() {
    let f = Fixture::new();
    let a = f.store(PROJECT_A);
    assert_eq!(f.vault.status().unwrap(), VaultStatus::NotCreated);
    assert!(matches!(a.get(&auth_id()), Err(SecretError::NotFound(_))));
    assert!(matches!(
        a.set(&auth_id(), &meta(), &s(SECRET)),
        Err(SecretError::Locked(StoreKind::Vault))
    ));
    assert!(!a.delete(&auth_id()).unwrap());
    assert!(matches!(
        f.vault.unlock(&s(MASTER)),
        Err(VaultError::NotCreated)
    ));
    assert!(matches!(
        f.vault.create(&s(""), KdfParams::insecure_for_tests()),
        Err(VaultError::EmptyPassword)
    ));
    assert!(!f.path().exists());
    f.vault
        .create(&s(MASTER), KdfParams::insecure_for_tests())
        .unwrap();
    assert_eq!(f.vault.status().unwrap(), VaultStatus::Unlocked);
    assert!(matches!(
        f.vault.create(&s(MASTER), KdfParams::insecure_for_tests()),
        Err(VaultError::AlreadyExists(_))
    ));
    f.vault.lock();
    assert_eq!(f.vault.status().unwrap(), VaultStatus::Locked);
    assert!(matches!(
        a.set(&auth_id(), &meta(), &s(SECRET)),
        Err(SecretError::Locked(StoreKind::Vault))
    ));
    assert!(matches!(
        a.delete(&auth_id()),
        Err(SecretError::Locked(StoreKind::Vault))
    ));
}

#[test]
fn wrong_password_is_typed_and_leaves_the_file_untouched() {
    let f = Fixture::created();
    f.store(PROJECT_A)
        .set(&auth_id(), &meta(), &s(SECRET))
        .unwrap();
    f.vault.lock();
    let before = f.bytes();
    let err = f.vault.unlock(&s("not the password")).unwrap_err();
    assert!(matches!(err, VaultError::WrongPassword), "{err}");
    assert_eq!(err.to_string(), "wrong master password");
    assert!(!f.vault.is_unlocked());
    assert_eq!(f.bytes(), before);
    f.vault.unlock(&s(MASTER)).unwrap();
}

/// Flipping any byte outside the KDF cost fields fails with `WrongPassword` (header prefix, key
/// check) or `Corrupted` / `NewerVersion` (structure, payload AEAD), never opens, never writes.
#[test]
fn every_flipped_byte_is_detected() {
    let f = Fixture::created();
    f.store(PROJECT_A)
        .set(&auth_id(), &meta(), &s(SECRET))
        .unwrap();
    f.vault.lock();
    let original = f.bytes();
    for i in (0..original.len()).filter(|i| !(12..24).contains(i)) {
        let mut tampered = original.clone();
        tampered[i] ^= 0x01;
        fs::write(f.path(), &tampered).unwrap();
        let err = f.vault.unlock(&s(MASTER)).unwrap_err();
        let expected = if (24..80).contains(&i) {
            matches!(err, VaultError::WrongPassword)
        } else {
            matches!(
                err,
                VaultError::Corrupted { .. } | VaultError::NewerVersion { .. }
            )
        };
        assert!(expected, "byte {i}: {err:?}");
        assert_eq!(f.bytes(), tampered, "byte {i}: file was modified");
        assert!(!f.vault.is_unlocked());
        // Wrong-password results count for the back-off; reset it between rounds.
        f.vault.state().throttle.record_success();
    }
    // A flipped ciphertext byte is an AEAD failure, reported as damage, not as a wrong password.
    let mut tampered = original.clone();
    let last = tampered.len() - 1;
    tampered[last] ^= 0x80;
    fs::write(f.path(), &tampered).unwrap();
    assert!(matches!(
        f.vault.unlock(&s(MASTER)),
        Err(VaultError::Corrupted {
            detail: "payload authentication failed",
            ..
        })
    ));
    fs::write(f.path(), &original).unwrap();
    f.vault.unlock(&s(MASTER)).unwrap();
}

#[test]
fn tampered_kdf_parameters_are_detected() {
    let f = Fixture::created();
    f.vault.lock();
    let original = f.bytes();
    for (at, value) in [(12usize, 16u32), (16, 2), (20, 2)] {
        let mut tampered = original.clone();
        tampered[at..at + 4].copy_from_slice(&value.to_le_bytes());
        fs::write(f.path(), &tampered).unwrap();
        let err = f.vault.unlock(&s(MASTER)).unwrap_err();
        assert!(
            matches!(
                err,
                VaultError::WrongPassword | VaultError::Corrupted { .. }
            ),
            "offset {at}: {err:?}"
        );
        assert_eq!(f.bytes(), tampered);
        f.vault.state().throttle.record_success();
    }
}

#[test]
fn truncated_extended_and_foreign_files_are_refused_untouched() {
    let f = Fixture::created();
    f.store(PROJECT_A)
        .set(&auth_id(), &meta(), &s(SECRET))
        .unwrap();
    f.vault.lock();
    let original = f.bytes();
    let mut cases: Vec<Vec<u8>> = [0, 4, 8, 50, HEADER_LEN - 1, HEADER_LEN, original.len() - 1]
        .iter()
        .map(|&n| original[..n].to_vec())
        .collect();
    let mut extended = original.clone();
    extended.extend_from_slice(b"extra");
    cases.push(extended);
    cases.push(br#"{"schema_version":1}"#.to_vec());
    for case in cases {
        fs::write(f.path(), &case).unwrap();
        let err = f.vault.unlock(&s(MASTER)).unwrap_err();
        assert!(
            matches!(err, VaultError::Corrupted { .. }),
            "len {}: {err:?}",
            case.len()
        );
        assert!(err.to_string().contains("left untouched"));
        assert_eq!(f.bytes(), case);
    }
    let mut newer = original;
    newer[8..10].copy_from_slice(&2u16.to_le_bytes());
    fs::write(f.path(), &newer).unwrap();
    assert!(matches!(
        f.vault.unlock(&s(MASTER)),
        Err(VaultError::NewerVersion {
            found: 2,
            supported: 1,
            ..
        })
    ));
    assert_eq!(f.bytes(), newer);
}

#[test]
fn kdf_parameters_are_persisted_in_the_header() {
    let f = Fixture::new();
    let params = KdfParams::new(KdfParams::MIN_M_COST_KIB, 2, 1).unwrap();
    f.vault.create(&s(MASTER), params).unwrap();
    let bytes = f.bytes();
    assert_eq!(&bytes[0..8], b"NSQVAULT");
    assert_eq!(u16::from_le_bytes([bytes[8], bytes[9]]), 1);
    assert_eq!((bytes[10], bytes[11]), (1, 1));
    assert_eq!(&bytes[12..16], &(19 * 1024u32).to_le_bytes());
    assert_eq!(&bytes[16..20], &2u32.to_le_bytes());
    assert_eq!(&bytes[20..24], &1u32.to_le_bytes());
    f.vault.lock();
    f.vault.unlock(&s(MASTER)).unwrap();
    assert_eq!(f.vault.kdf_params(), Some(params));

    // Floors and ceilings for new parameters.
    assert!(KdfParams::new(KdfParams::MIN_M_COST_KIB - 1, 3, 1).is_err());
    assert!(KdfParams::new(KdfParams::MAX_M_COST_KIB + 1, 3, 1).is_err());
    assert!(KdfParams::new(64 * 1024, 1, 1).is_err());
    assert!(KdfParams::new(64 * 1024, 17, 1).is_err());
    assert!(KdfParams::new(64 * 1024, 3, 0).is_err());
    assert!(KdfParams::new(64 * 1024, 3, 9).is_err());
    assert_eq!(KdfParams::new(64 * 1024, 3, 1).unwrap(), KdfParams::DEFAULT);
    assert_eq!(KdfParams::default(), KdfParams::DEFAULT);
}

/// ADR-0005's real parameters (64 MiB, t = 3, p = 1) work end to end.
#[test]
fn default_parameters_round_trip() {
    let f = Fixture::new();
    f.vault.create(&s(MASTER), KdfParams::DEFAULT).unwrap();
    f.store(PROJECT_A)
        .set(&auth_id(), &meta(), &s(SECRET))
        .unwrap();
    f.vault.lock();
    f.vault.unlock(&s(MASTER)).unwrap();
    assert_eq!(f.vault.kdf_params(), Some(KdfParams::DEFAULT));
    assert_eq!(
        f.store(PROJECT_A).get(&auth_id()).unwrap().expose_secret(),
        SECRET
    );
}

#[test]
fn change_master_password_re_encrypts_atomically() {
    let f = Fixture::created();
    let a = f.store(PROJECT_A);
    a.set(&auth_id(), &meta(), &s(SECRET)).unwrap();
    let before = f.bytes();

    // Wrong current password: refused, file untouched.
    assert!(matches!(
        f.vault
            .change_master_password(&s("nope"), &s(NEW_MASTER), None),
        Err(VaultError::WrongPassword)
    ));
    assert!(matches!(
        f.vault.change_master_password(&s(MASTER), &s(""), None),
        Err(VaultError::EmptyPassword)
    ));
    assert_eq!(f.bytes(), before);

    let stronger = KdfParams::new(KdfParams::MIN_M_COST_KIB, 2, 1).unwrap();
    f.vault
        .change_master_password(&s(MASTER), &s(NEW_MASTER), Some(stronger))
        .unwrap();
    let after = f.bytes();
    assert_ne!(after[24..40], before[24..40], "salt renewed");
    assert_eq!(&after[12..16], &(19 * 1024u32).to_le_bytes());
    assert_eq!(a.get(&auth_id()).unwrap().expose_secret(), SECRET);

    f.vault.lock();
    assert!(matches!(
        f.vault.unlock(&s(MASTER)),
        Err(VaultError::WrongPassword)
    ));
    f.vault.unlock(&s(NEW_MASTER)).unwrap();
    assert_eq!(a.get(&auth_id()).unwrap().expose_secret(), SECRET);
    assert_eq!(f.vault.kdf_params(), Some(stronger));
    assert_eq!(
        fs::metadata(f.path()).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn second_process_is_refused_by_the_workspace_lock() {
    let dir = tempfile::tempdir().unwrap();
    let lock = dir.path().join("ninesql.lock");
    // Another process holds the workspace lock (another open file description).
    let other = File::create(&lock).unwrap();
    other.try_lock().unwrap();
    let err = Vault::open_at(dir.path().join("vault.bin"), &lock).unwrap_err();
    assert!(
        matches!(err, VaultError::Storage(StorageError::Locked { .. })),
        "{err:?}"
    );
    drop(other);
    let vault = Vault::open_at(dir.path().join("vault.bin"), &lock).unwrap();
    // While the vault is open, another process cannot take the lock either.
    assert!(ninesql_storage::lock::is_locked_elsewhere(&lock).unwrap());
    // Inside this process the lock is shared (the workspace and the vault coexist).
    let _workspace = DirLock::acquire(&lock).unwrap();
    drop(vault);
}

#[test]
fn open_with_xdg_paths_creates_private_files() {
    let dir = tempfile::tempdir().unwrap();
    let xdg = ninesql_storage::XdgDirs {
        config_home: dir.path().join("config"),
        data_home: dir.path().join("data"),
        state_home: dir.path().join("state"),
    };
    let paths = Paths::new(&xdg);
    let vault = Vault::open(&paths).unwrap();
    assert_eq!(vault.path(), paths.vault_file());
    vault
        .create(&s(MASTER), KdfParams::insecure_for_tests())
        .unwrap();
    let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(paths.data_dir()), 0o700);
    assert_eq!(mode(&paths.vault_file()), 0o600);
    assert_eq!(mode(&paths.lock_file()), 0o600);
    // A loose mode is tightened on the next unlock (read through the descriptor).
    fs::set_permissions(paths.vault_file(), fs::Permissions::from_mode(0o644)).unwrap();
    vault.lock();
    vault.unlock(&s(MASTER)).unwrap();
    assert_eq!(mode(&paths.vault_file()), 0o600);
    // A symbolic link in place of vault.bin is refused, not followed.
    let target = dir.path().join("elsewhere");
    fs::rename(paths.vault_file(), &target).unwrap();
    std::os::unix::fs::symlink(&target, paths.vault_file()).unwrap();
    vault.lock();
    assert!(matches!(
        vault.unlock(&s(MASTER)),
        Err(VaultError::Storage(StorageError::WrongFileType { .. }))
    ));
}

#[test]
fn backoff_refuses_attempts_after_repeated_failures() {
    let f = Fixture::created();
    f.vault.lock();
    f.vault.set_unlock_backoff(UnlockBackoff {
        free_attempts: 2,
        base_delay: Duration::from_secs(3600),
        max_delay: Duration::from_secs(3600),
    });
    for _ in 0..2 {
        assert!(matches!(
            f.vault.unlock(&s("guess")),
            Err(VaultError::WrongPassword)
        ));
    }
    // The third failure starts the wait …
    assert!(matches!(
        f.vault.unlock(&s("guess")),
        Err(VaultError::WrongPassword)
    ));
    // … and even the right password is refused before any key derivation.
    let err = f.vault.unlock(&s(MASTER)).unwrap_err();
    assert!(
        matches!(err, VaultError::TooManyAttempts { retry_after } if retry_after > Duration::from_secs(3500)),
        "{err:?}"
    );
    assert!(matches!(
        f.vault
            .change_master_password(&s(MASTER), &s(NEW_MASTER), None),
        Err(VaultError::TooManyAttempts { .. })
    ));
    assert!(matches!(
        f.store(PROJECT_A).get(&auth_id()),
        Err(SecretError::Locked(StoreKind::Vault))
    ));
}

#[test]
fn idle_timeout_locks_the_vault() {
    let f = Fixture::created();
    f.vault.set_idle_timeout(Some(Duration::from_millis(200)));
    assert!(!f.vault.lock_if_idle());
    assert!(f.vault.is_unlocked());
    std::thread::sleep(Duration::from_millis(300));
    assert!(f.vault.lock_if_idle());
    assert_eq!(f.vault.status().unwrap(), VaultStatus::Locked);

    // Enforced at access time too, without the timer.
    f.vault.unlock(&s(MASTER)).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    assert!(matches!(
        f.store(PROJECT_A).get(&auth_id()),
        Err(SecretError::Locked(StoreKind::Vault))
    ));
    f.vault.set_idle_timeout(None);
    f.vault.unlock(&s(MASTER)).unwrap();
    std::thread::sleep(Duration::from_millis(50));
    assert!(!f.vault.lock_if_idle());
}

#[test]
fn failed_write_keeps_memory_consistent_with_the_file() {
    let f = Fixture::created();
    let a = f.store(PROJECT_A);
    a.set(&auth_id(), &meta(), &s("kept")).unwrap();
    // Make the atomic rename fail: a non-empty directory in place of vault.bin.
    let saved = f.dir.path().join("saved.bin");
    fs::rename(f.path(), &saved).unwrap();
    fs::create_dir(f.path()).unwrap();
    fs::write(f.path().join("x"), b"x").unwrap();
    assert!(a.set(&auth_id(), &meta(), &s("lost")).is_err());
    assert!(a.set(&id("other:auth"), &meta(), &s("lost")).is_err());
    assert!(a.delete(&auth_id()).is_err());
    assert_eq!(a.get(&auth_id()).unwrap().expose_secret(), "kept");
    assert_eq!(a.ids().unwrap(), vec![auth_id()]);
}

#[test]
fn size_limits() {
    let f = Fixture::created();
    let a = f.store(PROJECT_A);
    let big = "x".repeat(MAX_SECRET_LEN + 1);
    assert!(matches!(
        a.set(&auth_id(), &meta(), &s(&big)),
        Err(SecretError::Backend {
            store: StoreKind::Vault,
            ..
        })
    ));
    let long_label = SecretMeta {
        label: "l".repeat(MAX_LABEL_LEN + 1),
        ..meta()
    };
    assert!(a.set(&auth_id(), &long_label, &s(SECRET)).is_err());
    let max = "y".repeat(MAX_SECRET_LEN);
    a.set(&auth_id(), &meta(), &s(&max)).unwrap();
    f.vault.lock();
    f.vault.unlock(&s(MASTER)).unwrap();
    assert_eq!(a.get(&auth_id()).unwrap().expose_secret(), max);
}

/// Records every span and event field, to check that nothing secret is logged.
struct Capture(Arc<Mutex<String>>);

struct Fields<'a>(&'a mut String);

impl tracing::field::Visit for Fields<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn fmt::Debug) {
        use std::fmt::Write;
        let _ = write!(self.0, " {}={:?}", field.name(), value);
    }
}

impl tracing::Subscriber for Capture {
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

/// I1: no secret, master password or label in `vault.bin`, in any other file of the directory,
/// in the logs or in error messages.
#[test]
fn no_plaintext_in_the_file_logs_or_errors() {
    let logs = Arc::new(Mutex::new(String::new()));
    let f = Fixture::new();
    let mut errors = String::new();
    tracing::subscriber::with_default(Capture(logs.clone()), || {
        f.vault
            .create(&s(MASTER), KdfParams::insecure_for_tests())
            .unwrap();
        let a = f.store(PROJECT_A);
        a.set(&auth_id(), &meta(), &s(SECRET)).unwrap();
        f.vault.lock();
        errors.push_str(&f.vault.unlock(&s(NEW_MASTER)).unwrap_err().to_string());
        f.vault.unlock(&s(MASTER)).unwrap();
        assert_eq!(a.get(&auth_id()).unwrap().expose_secret(), SECRET);
        errors.push_str(
            &f.vault
                .change_master_password(&s(SECRET), &s(NEW_MASTER), None)
                .unwrap_err()
                .to_string(),
        );
        f.vault
            .change_master_password(&s(MASTER), &s(NEW_MASTER), None)
            .unwrap();
        let mut corrupt = f.bytes();
        let last = corrupt.len() - 1;
        corrupt[last] ^= 1;
        let good = f.bytes();
        fs::write(f.path(), &corrupt).unwrap();
        f.vault.lock();
        errors.push_str(&f.vault.unlock(&s(NEW_MASTER)).unwrap_err().to_string());
        fs::write(f.path(), &good).unwrap();
        f.vault.unlock(&s(NEW_MASTER)).unwrap();
        errors.push_str(&format!("{:?} {:?}", f.vault, a));
        f.vault.lock();
    });
    let logs = logs.lock().unwrap().clone();
    assert!(logs.contains("vault unlocked"), "capture works: {logs}");
    for needle in [MASTER, NEW_MASTER, SECRET, LABEL] {
        assert!(!logs.contains(needle), "{needle:?} in logs");
        assert!(!errors.contains(needle), "{needle:?} in errors");
        for entry in fs::read_dir(f.dir.path()).unwrap() {
            let path = entry.unwrap().path();
            let bytes = fs::read(&path).unwrap();
            assert!(
                !contains(&bytes, needle.as_bytes()),
                "{needle:?} in {}",
                path.display()
            );
        }
    }
    // The connection id and secret id are encrypted too.
    assert!(!contains(&f.bytes(), CONN.as_bytes()));
    // No temp file left next to the vault.
    let names: Vec<_> = fs::read_dir(f.dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    let mut names = names;
    names.sort();
    assert_eq!(names, ["ninesql.lock", "vault.bin"]);
}

#[test]
fn file_size_hides_secret_lengths() {
    let f = Fixture::created();
    let a = f.store(PROJECT_A);
    a.set(&auth_id(), &meta(), &s("a")).unwrap();
    let short = f.bytes().len();
    a.set(&auth_id(), &meta(), &s(&"b".repeat(60))).unwrap();
    assert_eq!(f.bytes().len(), short);
    assert_eq!((short - HEADER_LEN - TAG_LEN) % format::PAD_TO, 0);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(8))]

    #[test]
    fn vault_round_trip(
        secrets in proptest::collection::btree_map("[a-z0-9-]{1,20}:auth", "\\PC{1,80}", 0..6)
    ) {
        let f = Fixture::created();
        let a = f.store(PROJECT_A);
        for (k, v) in &secrets {
            a.set(&id(k), &meta(), &s(v)).unwrap();
        }
        f.vault.lock();
        f.vault.unlock(&s(MASTER)).unwrap();
        prop_assert_eq!(a.ids().unwrap().len(), secrets.len());
        let bytes = f.bytes();
        for (k, v) in &secrets {
            let got = a.get(&id(k)).unwrap();
            prop_assert_eq!(got.expose_secret(), v.as_str());
            if v.len() >= 8 {
                prop_assert!(!contains(&bytes, v.as_bytes()));
            }
        }
    }
}
