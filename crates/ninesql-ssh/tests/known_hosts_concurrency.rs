//! Concurrent writers of NineSQL's `known_hosts` never lose an entry (ROADMAP Phase 1
//! follow-ups, P1-D SSH): two processes (the test binary run again as a child) and two threads
//! of one process, each with its own `KnownHosts`, add entries at the same time.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // Tests may panic (AGENTS.md).

use std::path::Path;
use std::process::Command;

use ninesql_ssh::{HostKeyStatus, KnownHosts};

/// Set in a child process: the `known_hosts` path to write to.
const CHILD_PATH: &str = "NINESQL_TEST_KNOWN_HOSTS_CHILD_PATH";
/// Set in a child process: the prefix of the host names it adds.
const CHILD_PREFIX: &str = "NINESQL_TEST_KNOWN_HOSTS_CHILD_PREFIX";
/// Entries added by each writer.
const PER_WRITER: u8 = 25;

/// An `ssh-ed25519` public key blob (wire format) with a fixed 32-byte key.
fn blob(seed: u8) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&11u32.to_be_bytes());
    b.extend_from_slice(b"ssh-ed25519");
    b.extend_from_slice(&32u32.to_be_bytes());
    b.extend_from_slice(&[seed; 32]);
    b
}

fn write_entries(own: &Path, prefix: &str) {
    for i in 0..PER_WRITER {
        // A fresh load each time, as separate connects would do.
        let mut kh = KnownHosts::load(own, None).unwrap();
        kh.add(
            &format!("{prefix}-{i}.example.test"),
            22,
            "ssh-ed25519",
            &blob(i),
        )
        .unwrap();
    }
}

fn assert_all_present(own: &Path, prefixes: &[&str]) {
    let kh = KnownHosts::load(own, None).unwrap();
    for prefix in prefixes {
        for i in 0..PER_WRITER {
            let host = format!("{prefix}-{i}.example.test");
            assert!(
                matches!(kh.check(&host, 22, &blob(i)), HostKeyStatus::Known { .. }),
                "lost entry for {host}"
            );
        }
    }
    let lines = std::fs::read_to_string(own).unwrap().lines().count();
    assert_eq!(lines, prefixes.len() * usize::from(PER_WRITER));
}

/// The child role: does nothing unless run by `two_processes_do_not_lose_entries`.
#[test]
fn child_writer() {
    let (Some(path), Some(prefix)) = (
        std::env::var_os(CHILD_PATH),
        std::env::var(CHILD_PREFIX).ok(),
    ) else {
        return;
    };
    write_entries(Path::new(&path), &prefix);
}

#[test]
fn two_processes_do_not_lose_entries() {
    let tmp = tempfile::tempdir().unwrap();
    let own = tmp.path().join("ninesql").join("known_hosts");
    let exe = std::env::current_exe().unwrap();
    let children: Vec<_> = ["proc-a", "proc-b"]
        .iter()
        .map(|prefix| {
            Command::new(&exe)
                .args(["--exact", "child_writer", "--test-threads=1", "--quiet"])
                .env(CHILD_PATH, &own)
                .env(CHILD_PREFIX, prefix)
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap()
        })
        .collect();
    for mut child in children {
        let status = child.wait().unwrap();
        assert!(status.success(), "a writer process failed: {status}");
    }
    assert_all_present(&own, &["proc-a", "proc-b"]);
}

#[test]
fn two_threads_with_their_own_known_hosts_do_not_lose_entries() {
    let tmp = tempfile::tempdir().unwrap();
    let own = tmp.path().join("ninesql").join("known_hosts");
    std::thread::scope(|s| {
        for prefix in ["thread-a", "thread-b"] {
            let own = &own;
            s.spawn(move || write_entries(own, prefix));
        }
    });
    assert_all_present(&own, &["thread-a", "thread-b"]);
}
