//! `project.json` identity (N6), the workspace lock (L8) and directory checks (L2).
#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests may panic (AGENTS.md).

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use ninesql_core::{ProjectFile, ProjectId, Workspace, WorkspaceError};
use ninesql_storage::document::{self, backup_path};
use ninesql_storage::{Paths, ProjectName, StorageError, XdgDirs};

fn paths(root: &Path) -> Paths {
    Paths::new(&XdgDirs {
        config_home: root.join("config"),
        data_home: root.join("data"),
        state_home: root.join("state"),
    })
}

fn fixture(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(rel)
}

#[test]
fn new_project_gets_a_stable_id() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = Workspace::open(paths(tmp.path()), ProjectName::default_project()).unwrap();
    let id = ws.project_id();
    let file = document::load::<ProjectFile>(&ws.project_paths().project_file())
        .unwrap()
        .unwrap();
    assert_eq!(file.document.id, id);
    assert_eq!(file.migrated_from, None);
    drop(ws);
    let again = Workspace::open(paths(tmp.path()), ProjectName::default_project()).unwrap();
    assert_eq!(again.project_id(), id);
    let text = fs::read_to_string(again.project_paths().project_file()).unwrap();
    assert!(
        text.starts_with(&format!(
            "{{\n  \"schema_version\": 2,\n  \"id\": \"{id}\",\n  \"name\": \"General\""
        )),
        "{text}"
    );
}

#[test]
fn project_v1_fixture_migrates_to_an_id() {
    let bytes = fs::read(fixture("project/v1.json")).unwrap();
    let loaded = document::decode::<ProjectFile>(&fixture("project/v1.json"), &bytes).unwrap();
    assert_eq!(loaded.migrated_from, Some(1));
    assert_eq!(loaded.document.name.as_str(), "General");
    assert_eq!(
        loaded.document.description.as_deref(),
        Some("Fake project used as a fixture")
    );
    assert_eq!(loaded.document.id.as_uuid().get_version_num(), 4);

    // On disk: migrated once with a backup, then the id never changes.
    let tmp = tempfile::tempdir().unwrap();
    let p = paths(tmp.path());
    let project = ProjectName::default_project();
    let dir = p.project(&project);
    fs::create_dir_all(dir.dir()).unwrap();
    fs::copy(fixture("project/v1.json"), dir.project_file()).unwrap();
    let ws = Workspace::open(p.clone(), project.clone()).unwrap();
    let id = ws.project_id();
    // JSON backups are re-encoded from the scrubbed value: same content as the original.
    let bak: serde_json::Value =
        serde_json::from_slice(&fs::read(backup_path(&dir.project_file())).unwrap()).unwrap();
    assert_eq!(
        bak,
        serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()
    );
    drop(ws);
    assert_eq!(Workspace::open(p, project).unwrap().project_id(), id);
}

/// Renaming the project (directory and name) keeps the id that scopes its keyring items.
#[test]
fn renamed_project_keeps_its_id() {
    let tmp = tempfile::tempdir().unwrap();
    let p = paths(tmp.path());
    let old = ProjectName::new("Old").unwrap();
    let id = Workspace::open(p.clone(), old.clone())
        .unwrap()
        .project_id();
    let new = ProjectName::new("New").unwrap();
    fs::rename(p.project(&old).dir(), p.project(&new).dir()).unwrap();
    assert_eq!(Workspace::open(p, new).unwrap().project_id(), id);
    assert_ne!(id, ProjectId::new_random());
}

#[test]
fn the_workspace_lock_is_shared_in_process_and_refused_elsewhere() {
    let tmp = tempfile::tempdir().unwrap();
    let p = paths(tmp.path());
    let a = Workspace::open(p.clone(), ProjectName::default_project()).unwrap();
    // Same process: a second project (or the same one) opens fine.
    let b = Workspace::open(p.clone(), ProjectName::new("Other").unwrap()).unwrap();
    assert!(ninesql_storage::lock::is_locked_elsewhere(&p.lock_file()).unwrap());
    drop((a, b));
    assert!(!ninesql_storage::lock::is_locked_elsewhere(&p.lock_file()).unwrap());

    // Another holder of the lock (as a second NineSQL process would be).
    let other = fs::File::open(p.lock_file()).unwrap();
    other.try_lock().unwrap();
    let err = Workspace::open(p.clone(), ProjectName::default_project()).unwrap_err();
    assert!(
        matches!(err, WorkspaceError::Storage(StorageError::Locked { .. })),
        "{err}"
    );
    drop(other);
    Workspace::open(p, ProjectName::default_project()).unwrap();
}

#[test]
fn symlinked_project_directory_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let p = paths(tmp.path());
    drop(Workspace::open(p.clone(), ProjectName::default_project()).unwrap());
    let elsewhere = tmp.path().join("elsewhere");
    fs::create_dir(&elsewhere).unwrap();
    fs::set_permissions(&elsewhere, fs::Permissions::from_mode(0o755)).unwrap();
    let evil = ProjectName::new("Evil").unwrap();
    std::os::unix::fs::symlink(&elsewhere, p.project(&evil).dir()).unwrap();
    let err = Workspace::open(p, evil).unwrap_err();
    assert!(
        matches!(
            err,
            WorkspaceError::Storage(StorageError::WrongFileType { .. })
        ),
        "{err}"
    );
    assert_eq!(
        fs::metadata(&elsewhere).unwrap().permissions().mode() & 0o777,
        0o755
    );
    assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0);
}
