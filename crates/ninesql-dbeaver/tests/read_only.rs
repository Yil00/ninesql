//! I4: the importer never creates, modifies, moves, deletes or locks anything under a DBeaver
//! directory.
//!
//! Three complementary checks:
//! 1. the whole pipeline runs on a copy whose files are `0444` and directories `0555` (a write
//!    attempt fails for a normal user);
//! 2. a snapshot of the tree (names, sizes, modification times, content) is identical before and
//!    after, including the `.bak` files the importer must ignore (this also covers runs as root,
//!    which bypasses permission bits);
//! 3. the sources of the crate contain no write, create, rename, delete or lock call, and only
//!    `src/fsread.rs` touches `std::fs`; the one exception is the export writer
//!    (`src/export/write.rs`, P5-D), which only creates new paths outside DBeaver directories
//!    (`tests/export_roundtrip.rs`).

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use ninesql_dbeaver::{SearchRoots, detect_workspaces, import_project, open_folder};

type Snapshot = BTreeMap<PathBuf, (bool, u64, Option<SystemTime>, Vec<u8>)>;

fn snapshot(root: &Path) -> Snapshot {
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

fn walk(root: &Path, dir: &Path, out: &mut Snapshot) {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()))
        .flatten()
        .collect();
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let meta = fs::symlink_metadata(&path).unwrap_or_else(|e| panic!("stat: {e}"));
        let rel = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
        if meta.is_dir() {
            out.insert(rel, (true, 0, meta.modified().ok(), Vec::new()));
            walk(root, &path, out);
        } else {
            let content = fs::read(&path).unwrap_or_default();
            out.insert(rel, (false, meta.len(), meta.modified().ok(), content));
        }
    }
}

fn set_tree_mode(dir: &Path, file_mode: u32, dir_mode: u32) {
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            set_tree_mode(&path, file_mode, dir_mode);
            let _ = fs::set_permissions(&path, fs::Permissions::from_mode(dir_mode));
        } else {
            let _ = fs::set_permissions(&path, fs::Permissions::from_mode(file_mode));
        }
    }
    let _ = fs::set_permissions(dir, fs::Permissions::from_mode(dir_mode));
}

/// A fake home with the 26.2.4 fixture at the native and Flatpak locations, plus `.bak` files.
fn fake_home(tmp: &Path) -> PathBuf {
    let fixtures = common::fixture_workspaces();
    let (_, ws) = fixtures
        .iter()
        .find(|(v, _)| v == "26.2.4")
        .unwrap_or_else(|| panic!("26.2.4 fixture missing"));
    let home = tmp.join("home");
    for rel in [
        ".local/share/DBeaverData/workspace6",
        ".var/app/io.dbeaver.DBeaverCommunity/data/DBeaverData/workspace6",
    ] {
        let target = home.join(rel);
        common::copy_tree(ws, &target).unwrap_or_else(|e| panic!("copy: {e}"));
        let dot = target.join("General").join(".dbeaver");
        // Backups the importer must ignore: invalid JSON, so reading them would show up.
        fs::write(dot.join(".data-sources.json.bak"), "{ not json").unwrap_or_default();
        fs::write(dot.join(".credentials-config.json.bak"), "garbage").unwrap_or_default();
        fs::write(dot.join("data-sources.json.bak"), "{ not json").unwrap_or_default();
    }
    home
}

#[test]
fn import_never_writes_under_dbeaver_directories() {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let home = fake_home(tmp.path());
    let dbeaver_roots = [home.join(".local"), home.join(".var")];
    set_tree_mode(&home, 0o444, 0o555);
    let before: Vec<Snapshot> = dbeaver_roots.iter().map(|r| snapshot(r)).collect();

    let workspaces = detect_workspaces(&SearchRoots { home: home.clone() });
    assert_eq!(workspaces.len(), 2, "native and Flatpak workspaces");
    let mut imported = 0;
    for ws in &workspaces {
        assert_eq!(
            ws.product_version
                .as_deref()
                .map(|v| v.starts_with("26.2.4")),
            Some(true)
        );
        for project in &ws.projects {
            assert_eq!(
                project.data_source_files.len(),
                1,
                ".bak files must be ignored"
            );
            let p = import_project(project);
            assert!(
                p.issues
                    .iter()
                    .all(|i| i.kind != ninesql_dbeaver::IssueKind::File),
                "a backup file was read: {:?}",
                p.issues
            );
            imported += p.connections.len();
        }
        // Picking the workspace or the project folder by hand works the same.
        let picked = open_folder(&ws.path).unwrap_or_else(|e| panic!("open_folder: {e}"));
        assert_eq!(picked.projects, ws.projects);
        let single = open_folder(&ws.projects[0].path).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(single.projects, ws.projects);
    }
    assert_eq!(imported, 20);

    let after: Vec<Snapshot> = dbeaver_roots.iter().map(|r| snapshot(r)).collect();
    assert_eq!(before, after, "the DBeaver trees changed during the import");
    set_tree_mode(&home, 0o644, 0o755);
}

#[test]
fn detection_skips_missing_and_empty_locations() {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let home = tmp.path().join("home");
    fs::create_dir_all(home.join("snap/dbeaver-ce/current/.local/share/DBeaverData/workspace6"))
        .unwrap_or_default();
    let before = snapshot(tmp.path());
    assert!(detect_workspaces(&SearchRoots { home: home.clone() }).is_empty());
    assert!(open_folder(&home).is_err());
    assert!(open_folder(&home.join("missing")).is_err());
    assert_eq!(before, snapshot(tmp.path()), "detection created something");
}

#[test]
fn sources_have_no_write_path() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let forbidden = [
        "OpenOptions",
        "fs::write",
        "File::create",
        "create_dir",
        "remove_file",
        "remove_dir",
        "fs::rename",
        "fs::copy",
        "hard_link",
        "symlink(",
        "set_permissions",
        "set_len",
        "write_all",
        "flock",
        "try_lock",
        "lock_exclusive",
        "lock_shared",
        "tempfile",
        "persist(",
    ];
    // Every `.rs` file, subdirectories included (`src/export/`).
    let mut files = Vec::new();
    let mut dirs = vec![src.clone()];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("{e}"))
            .flatten()
        {
            let path = entry.path();
            if path.is_dir() {
                dirs.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                files.push(path);
            }
        }
    }
    assert!(files.iter().any(|f| f.ends_with("export/mod.rs")));
    for path in files {
        let name = path
            .strip_prefix(&src)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();
        let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{name}: {e}"));
        // Doc comments may mention the forbidden calls; only code lines are checked.
        let code: String = text
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        // The export writer creates new files (O_EXCL), links and removes its own temporary
        // file: the only writing code of the crate.
        if name == "export/write.rs" {
            continue;
        }
        for f in forbidden {
            assert!(!code.contains(f), "{name} contains `{f}`");
        }
        if name != "fsread.rs" {
            assert!(
                !code.contains("std::fs") && !code.contains("File::open"),
                "{name} accesses the filesystem outside fsread.rs"
            );
        }
    }
}
