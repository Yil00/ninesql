//! Helpers shared by the integration tests. Test output names connection names, ids and keys,
//! never credential values (I1 applies to test output too).
#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};

use aes::cipher::{BlockEncryptMut, KeyIvInit, block_padding::Pkcs7};

/// Fake secrets of the P0-E fixtures (tests/fixtures/README.md). They all contain this marker,
/// which must never appear in any `Debug`, `Display`, issue or log output.
pub const SECRET_MARKER: &str = "not-a-secret";

/// DBeaver Community's public fixed key (test-only copy, to build synthetic credential files).
const DBEAVER_KEY: [u8; 16] = [
    0xba, 0xbb, 0x4a, 0x9f, 0x77, 0x4a, 0xb8, 0x53, 0xc9, 0x6c, 0x2d, 0x65, 0x3d, 0xfe, 0x54, 0x4a,
];

/// Encrypts `plaintext` like DBeaver Community (IV prefix + AES-128-CBC + PKCS#7).
pub fn encrypt_like_dbeaver(plaintext: &[u8], iv: [u8; 16]) -> Vec<u8> {
    encrypt_with_key(plaintext, DBEAVER_KEY, iv)
}

/// Same scheme with another key: stands for a file DBeaver Community cannot read either
/// (DBeaver PRO master password, a different product build).
pub fn encrypt_with_key(plaintext: &[u8], key: [u8; 16], iv: [u8; 16]) -> Vec<u8> {
    let ct = cbc::Encryptor::<aes::Aes128>::new(&key.into(), &iv.into())
        .encrypt_padded_vec_mut::<Pkcs7>(plaintext);
    let mut out = iv.to_vec();
    out.extend_from_slice(&ct);
    out
}

pub fn fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

/// `(version, workspace6 dir)` for every `dbeaver-<version>` fixture, sorted.
pub fn fixture_workspaces() -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(fixtures_root()) else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some(version) = name.strip_prefix("dbeaver-")
            && entry.path().is_dir()
        {
            out.push((version.to_owned(), entry.path().join("workspace6")));
        }
    }
    out.sort();
    out
}

/// Recursively copies `src` into `dst` (created).
pub fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let target = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

/// Writes a minimal DBeaver project (`<dir>/<name>/.dbeaver/…`) and returns its folder.
pub fn write_project(
    dir: &Path,
    name: &str,
    data_sources: &str,
    credentials: Option<&[u8]>,
) -> std::io::Result<PathBuf> {
    let project = dir.join(name);
    let dot = project.join(".dbeaver");
    fs::create_dir_all(&dot)?;
    fs::write(dot.join("data-sources.json"), data_sources)?;
    if let Some(c) = credentials {
        fs::write(dot.join("credentials-config.json"), c)?;
    }
    Ok(project)
}
