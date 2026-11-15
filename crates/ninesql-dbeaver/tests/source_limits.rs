//! Whole-source limits, read-once and cancellation (P2-D review, M1). Synthetic content only:
//! hosts under `example.test`, made-up users and passwords.
//!
//! The crafted case of the review: one project of many connections plus many symbolic links to
//! it took 15 s and 3.2 GB before this fix (every link was read as a project of its own).

mod common;

use std::fmt::Write as _;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::time::{Duration, Instant};

use ninesql_dbeaver::{
    CancelFlag, DbeaverError, InstallKind, IssueKind, MAX_DETECT_DIRS, MAX_LISTED_DUPLICATES,
    MAX_SOURCE_PROJECTS, SearchRoots, SourceImport, SourceLimits, Workspace, detect_workspaces,
    detect_workspaces_with, import_project, import_project_with, import_source,
    import_source_with_limits, open_folder, open_folder_with,
};

/// A minimal `data-sources.json` with `n` PostgreSQL connections whose ids start with `prefix`.
fn data_sources(prefix: &str, n: usize) -> String {
    let mut s = String::from(r#"{"folders":{},"connections":{"#);
    for i in 0..n {
        if i > 0 {
            s.push(',');
        }
        let _ = write!(
            s,
            r#""postgres-jdbc-{prefix}-{i}":{{"provider":"postgresql","driver":"postgres-jdbc",
            "name":"{prefix}-{i}","configuration":{{"host":"db{i}.example.test","port":"5432",
            "database":"nine","user":"nine_user","type":"dev"}}}}"#
        );
    }
    s.push_str("}}");
    s
}

fn write_project(dir: &Path, prefix: &str, n: usize) {
    let dot = dir.join(".dbeaver");
    fs::create_dir_all(&dot).unwrap_or_else(|e| panic!("{e}"));
    fs::write(dot.join("data-sources.json"), data_sources(prefix, n))
        .unwrap_or_else(|e| panic!("{e}"));
}

fn total_connections(s: &SourceImport) -> usize {
    s.projects.iter().map(|p| p.import.connections.len()).sum()
}

fn count(s: &SourceImport, kind: IssueKind) -> usize {
    s.issues.iter().filter(|i| i.kind == kind).count()
}

/// One project plus links to it, and a `..` path to it: the crafted folder of the review.
fn crafted_workspace(root: &Path, connections: usize, links: usize) {
    write_project(&root.join("General"), "nine", connections);
    for i in 0..links {
        symlink(root.join("General"), root.join(format!("link-{i:04}")))
            .unwrap_or_else(|e| panic!("{e}"));
    }
}

#[test]
fn symlinked_projects_are_read_once_and_reported() {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let ws = tmp.path().join("ws");
    crafted_workspace(&ws, 200, 100);
    let start = Instant::now();
    let workspace = open_folder(&ws).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(workspace.projects.len(), 1, "links are not listed twice");
    assert_eq!(workspace.duplicate_projects.len(), 100);
    let s = import_source(vec![workspace], &CancelFlag::new()).unwrap_or_else(|e| panic!("{e}"));
    let elapsed = start.elapsed();

    assert_eq!(s.projects.len(), 1, "one canonical project");
    assert_eq!(s.projects[0].project.name, "General");
    assert_eq!(total_connections(&s), 200);
    assert_eq!(count(&s, IssueKind::Duplicate), 100);
    assert!(!s.truncated);
    assert_eq!(s.workspaces.len(), 1);
    assert_eq!(s.workspaces[0].projects.len(), 1);
    assert!(elapsed < Duration::from_secs(10), "took {elapsed:?}");
}

#[test]
fn many_symlinks_stay_bounded_and_are_counted() {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let ws = tmp.path().join("ws");
    let links = MAX_LISTED_DUPLICATES + 44;
    crafted_workspace(&ws, 50, links);
    let start = Instant::now();
    let workspace = open_folder(&ws).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(workspace.projects.len(), 1);
    assert_eq!(workspace.duplicate_projects.len(), MAX_LISTED_DUPLICATES);
    assert_eq!(workspace.more_duplicates, 44);
    assert!(!workspace.listing_truncated, "duplicates are not a cut");
    let s = import_source(vec![workspace], &CancelFlag::new()).unwrap_or_else(|e| panic!("{e}"));
    let elapsed = start.elapsed();

    assert_eq!(s.projects.len(), 1);
    assert_eq!(total_connections(&s), 50, "bounded, not 50 x {links}");
    assert_eq!(count(&s, IssueKind::Duplicate), MAX_LISTED_DUPLICATES + 1);
    assert!(
        s.issues
            .iter()
            .any(|i| i.message.contains("44 more project folders"))
    );
    assert!(!s.truncated);
    assert!(elapsed < Duration::from_secs(10), "took {elapsed:?}");
}

#[test]
fn more_than_256_projects_cut_the_listing_and_it_is_reported() {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let ws = tmp.path().join("ws");
    for i in 0..MAX_SOURCE_PROJECTS + 4 {
        write_project(&ws.join(format!("p{i:04}")), &format!("p{i}"), 1);
    }
    let workspace = open_folder(&ws).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(workspace.projects.len(), MAX_SOURCE_PROJECTS);
    assert!(workspace.listing_truncated, "the flag the core must report");
    let s = import_source(vec![workspace], &CancelFlag::new()).unwrap_or_else(|e| panic!("{e}"));
    assert!(s.truncated);
    assert_eq!(s.projects.len(), MAX_SOURCE_PROJECTS);
    let cut = s
        .issues
        .iter()
        .find(|i| i.kind == IssueKind::Truncated && i.message.contains("was cut"))
        .unwrap_or_else(|| panic!("the cut listing is reported"));
    assert!(!cut.message.contains("  "), "{}", cut.message);
    for issue in &s.issues {
        assert!(!issue.message.contains("  "), "{}", issue.message);
    }
}

#[test]
fn links_to_a_large_non_project_folder_cost_one_listing() {
    // The review case at a smaller scale: many links to a folder whose `.dbeaver` holds many
    // files that are not data-sources files.
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let ws = tmp.path().join("ws");
    let dot = ws.join("junk/.dbeaver");
    fs::create_dir_all(&dot).unwrap_or_else(|e| panic!("{e}"));
    for i in 0..5_000 {
        fs::write(dot.join(format!("f{i}")), b"").unwrap_or_else(|e| panic!("{e}"));
    }
    for i in 0..1_000 {
        symlink(ws.join("junk"), ws.join(format!("link-{i:04}"))).unwrap_or_else(|e| panic!("{e}"));
    }
    let start = Instant::now();
    let res = open_folder(&ws);
    let elapsed = start.elapsed();
    assert!(matches!(res, Err(DbeaverError::NotDbeaver { .. })));
    assert!(elapsed < Duration::from_secs(5), "took {elapsed:?}");
}

#[test]
fn the_detection_budget_cuts_the_listing_and_it_is_reported() {
    // More distinct candidate folders with a `.dbeaver` directory than one detection lists.
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let ws = tmp.path().join("ws");
    write_project(&ws.join("a-real"), "real", 2);
    for i in 0..MAX_DETECT_DIRS + 10 {
        fs::create_dir_all(ws.join(format!("b{i:05}/.dbeaver"))).unwrap_or_else(|e| panic!("{e}"));
    }
    let start = Instant::now();
    let workspace = open_folder(&ws).unwrap_or_else(|e| panic!("{e}"));
    assert!(workspace.listing_truncated);
    assert_eq!(workspace.projects.len(), 1);
    let s = import_source(vec![workspace], &CancelFlag::new()).unwrap_or_else(|e| panic!("{e}"));
    assert!(s.truncated);
    assert_eq!(total_connections(&s), 2);
    assert!(start.elapsed() < Duration::from_secs(10));
}

#[test]
fn cancelled_detection_returns_cancelled() {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let ws = tmp.path().join("ws");
    write_project(&ws.join("General"), "nine", 1);
    let cancel = CancelFlag::new();
    cancel.cancel();
    assert!(matches!(
        open_folder_with(&ws, &cancel),
        Err(DbeaverError::Cancelled)
    ));
    let home = tmp.path().join("home");
    write_project(
        &home.join(".local/share/DBeaverData/workspace6/General"),
        "nine",
        1,
    );
    let roots = SearchRoots { home };
    assert!(matches!(
        detect_workspaces_with(&roots, &cancel),
        Err(DbeaverError::Cancelled)
    ));
    assert_eq!(detect_workspaces(&roots).len(), 1);
    assert!(open_folder_with(&ws, &CancelFlag::new()).is_ok());
}

#[test]
fn pseudo_files_are_not_read_and_exhaust_the_byte_budget() {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let ws = tmp.path().join("ws");
    write_project(&ws.join("General"), "nine", 2);
    let dot = ws.join("General/.dbeaver");
    for i in 0..60 {
        symlink(
            "/proc/self/pagemap",
            dot.join(format!("data-sources-{i:02}.json")),
        )
        .unwrap_or_else(|e| panic!("{e}"));
    }
    symlink("/proc/self/status", dot.join("data-sources-zz.json"))
        .unwrap_or_else(|e| panic!("{e}"));
    let start = Instant::now();
    let workspace = open_folder(&ws).unwrap_or_else(|e| panic!("{e}"));
    let s = import_source(vec![workspace], &CancelFlag::new()).unwrap_or_else(|e| panic!("{e}"));
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "took {:?}",
        start.elapsed()
    );
    assert_eq!(total_connections(&s), 2);
    let files = &s.projects[0].import;
    assert_eq!(files.source_files.len(), 1);
    assert_eq!(
        files
            .issues
            .iter()
            .filter(|i| i.kind == IssueKind::File && i.message.contains("special filesystem"))
            .count(),
        61
    );
}

#[test]
fn a_per_project_truncation_sets_truncated() {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let ws = tmp.path().join("ws");
    write_project(&ws.join("General"), "nine", 1);
    let dot = ws.join("General/.dbeaver");
    for i in 0..70 {
        fs::write(dot.join(format!("data-sources-{i:02}.json")), "{}")
            .unwrap_or_else(|e| panic!("{e}"));
    }
    let workspace = open_folder(&ws).unwrap_or_else(|e| panic!("{e}"));
    let s = import_source(vec![workspace], &CancelFlag::new()).unwrap_or_else(|e| panic!("{e}"));
    assert!(s.issues.is_empty());
    assert!(s.truncated, "a project-level cut counts");
}

#[test]
fn a_duplicate_of_a_project_not_read_says_so() {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let ws = tmp.path().join("ws");
    write_project(&ws.join("A"), "a", 1);
    write_project(&ws.join("B"), "b", 1);
    symlink(ws.join("B"), ws.join("link-b")).unwrap_or_else(|e| panic!("{e}"));
    let workspace = open_folder(&ws).unwrap_or_else(|e| panic!("{e}"));
    let s = import_source_with_limits(
        vec![workspace],
        SourceLimits::default().with_max_projects(1),
        &CancelFlag::new(),
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let dup = s
        .issues
        .iter()
        .find(|i| i.kind == IssueKind::Duplicate)
        .unwrap_or_else(|| panic!("duplicate reported"));
    assert!(dup.message.contains("was not read"), "{}", dup.message);
}

#[test]
fn dot_dot_paths_and_linked_dot_dbeaver_folders_are_duplicates() {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let ws = tmp.path().join("ws");
    write_project(&ws.join("A"), "a", 3);
    // Another project folder whose `.dbeaver` is a link to A's: the same files.
    fs::create_dir_all(ws.join("B")).unwrap_or_else(|e| panic!("{e}"));
    symlink(ws.join("A/.dbeaver"), ws.join("B/.dbeaver")).unwrap_or_else(|e| panic!("{e}"));
    let a = open_folder(&ws.join("A")).unwrap_or_else(|e| panic!("{e}"));
    // The same project through `..`.
    let a_again = open_folder(&ws.join("B/../A")).unwrap_or_else(|e| panic!("{e}"));
    let whole = open_folder(&ws).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(whole.projects.len(), 1);
    assert_eq!(whole.duplicate_projects.len(), 1);
    let s = import_source(vec![a, a_again, whole], &CancelFlag::new())
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(s.projects.len(), 1);
    assert_eq!(total_connections(&s), 3);
    assert_eq!(count(&s, IssueKind::Duplicate), 3);
}

#[test]
fn a_workspace_found_twice_through_detection_is_read_once() {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let home = tmp.path();
    let native = home.join(".local/share/DBeaverData/workspace6");
    write_project(&native.join("General"), "nine", 4);
    fs::create_dir_all(native.join(".metadata")).unwrap_or_else(|e| panic!("{e}"));
    let flatpak_data = home.join(".var/app/io.dbeaver.DBeaverCommunity/data/DBeaverData");
    fs::create_dir_all(&flatpak_data).unwrap_or_else(|e| panic!("{e}"));
    symlink(&native, flatpak_data.join("workspace6")).unwrap_or_else(|e| panic!("{e}"));
    let found = detect_workspaces(&SearchRoots {
        home: home.to_path_buf(),
    });
    assert_eq!(found.len(), 2);
    let s = import_source(found, &CancelFlag::new()).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(s.workspaces.len(), 1);
    assert_eq!(s.workspaces[0].kind, InstallKind::Native);
    assert_eq!(s.projects.len(), 1);
    assert_eq!(total_connections(&s), 4);
    assert_eq!(count(&s, IssueKind::Duplicate), 1);
}

fn four_projects(root: &Path, per_project: usize) -> Workspace {
    for name in ["p1", "p2", "p3", "p4"] {
        write_project(&root.join(name), name, per_project);
    }
    open_folder(root).unwrap_or_else(|e| panic!("{e}"))
}

#[test]
fn the_connection_limit_spans_projects() {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let ws = four_projects(&tmp.path().join("ws"), 30);
    let limits = SourceLimits::default().with_max_connections(50);
    let s = import_source_with_limits(vec![ws], limits, &CancelFlag::new())
        .unwrap_or_else(|e| panic!("{e}"));
    assert!(s.truncated);
    assert_eq!(total_connections(&s), 50);
    assert_eq!(s.projects.len(), 2);
    assert_eq!(s.projects[1].import.connections.len(), 20);
    assert!(
        s.projects[1]
            .import
            .issues
            .iter()
            .any(|i| i.kind == IssueKind::Truncated
                && i.message.contains("50 connections for the whole source"))
    );
    let not_read = s
        .issues
        .iter()
        .find(|i| i.kind == IssueKind::Truncated)
        .unwrap_or_else(|| panic!("projects not read are reported"));
    assert!(not_read.message.contains("2 project(s) not read"));
    assert!(not_read.message.contains("`p3`") && not_read.message.contains("`p4`"));
}

#[test]
fn the_project_limit_spans_workspaces() {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let a = four_projects(&tmp.path().join("a"), 1);
    let b = four_projects(&tmp.path().join("b"), 1);
    let limits = SourceLimits::default().with_max_projects(5);
    let s = import_source_with_limits(vec![a, b], limits, &CancelFlag::new())
        .unwrap_or_else(|e| panic!("{e}"));
    assert!(s.truncated);
    assert_eq!(s.projects.len(), 5);
    assert_eq!(s.workspaces[1].projects.len(), 1);
    assert!(
        s.issues
            .iter()
            .any(|i| i.message.contains("5 projects") && i.message.contains("3 project(s)"))
    );
}

#[test]
fn the_byte_limit_spans_projects_and_fails_closed() {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let ws = four_projects(&tmp.path().join("ws"), 5);
    let one = fs::metadata(tmp.path().join("ws/p1/.dbeaver/data-sources.json"))
        .unwrap_or_else(|e| panic!("{e}"))
        .len();
    let limits = SourceLimits::default().with_max_bytes(one * 2 + one / 2);
    let s = import_source_with_limits(vec![ws], limits, &CancelFlag::new())
        .unwrap_or_else(|e| panic!("{e}"));
    assert!(s.truncated);
    // p1 and p2 fit; p3's file is larger than what is left, so it is not read at all.
    assert_eq!(total_connections(&s), 10);
    assert_eq!(s.projects.len(), 3);
    assert!(s.projects[2].import.connections.is_empty());
    assert!(
        s.projects[2]
            .import
            .issues
            .iter()
            .any(|i| i.kind == IssueKind::Truncated
                && i.message.contains("bytes read for the whole source"))
    );
    assert!(
        s.issues
            .iter()
            .any(|i| i.message.contains("bytes read") && i.message.contains("`p4`"))
    );
}

#[test]
fn the_report_entry_limit_spans_projects() {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let ws = four_projects(&tmp.path().join("ws"), 3);
    let limits = SourceLimits::default().with_max_issues(1);
    let s = import_source_with_limits(vec![ws], limits, &CancelFlag::new())
        .unwrap_or_else(|e| panic!("{e}"));
    assert!(s.truncated);
    assert_eq!(s.projects.len(), 1, "the first project used the budget");
    assert!(
        s.issues
            .iter()
            .any(|i| i.message.contains("1 report entries") && i.message.contains("3 project(s)"))
    );
}

#[test]
fn a_set_cancel_flag_stops_the_read() {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let ws = four_projects(&tmp.path().join("ws"), 3);
    let cancel = CancelFlag::new();
    cancel.cancel();
    let project = ws.projects[0].clone();
    assert!(matches!(
        import_source(vec![ws], &cancel),
        Err(DbeaverError::Cancelled)
    ));
    assert!(matches!(
        import_project_with(&project, &cancel),
        Err(DbeaverError::Cancelled)
    ));
    // Not cancelled: the same result as `import_project`.
    let with = import_project_with(&project, &CancelFlag::new()).unwrap_or_else(|e| panic!("{e}"));
    let plain = import_project(&project);
    assert_eq!(with.connections.len(), plain.connections.len());
    assert_eq!(with.issues, plain.issues);
}

#[test]
fn a_cancel_from_another_thread_stops_a_long_read() {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let ws = tmp.path().join("ws");
    for i in 0..40 {
        write_project(&ws.join(format!("p{i:02}")), &format!("p{i}"), 200);
    }
    let workspace = open_folder(&ws).unwrap_or_else(|e| panic!("{e}"));
    let cancel = CancelFlag::new();
    let reader = {
        let cancel = cancel.clone();
        std::thread::spawn(move || import_source(vec![workspace], &cancel))
    };
    cancel.cancel();
    // Either it was cancelled, or it finished before the flag was seen; never a partial result
    // after a cancel was observed.
    match reader
        .join()
        .unwrap_or_else(|_| panic!("the reader panicked"))
    {
        Err(DbeaverError::Cancelled) => {}
        Ok(s) => assert_eq!(total_connections(&s), 8_000),
        Err(e) => panic!("unexpected error: {e}"),
    }
}

#[test]
fn fixtures_read_through_the_source_match_import_project() {
    for (version, ws) in common::fixture_workspaces() {
        let workspace = open_folder(&ws).unwrap_or_else(|e| panic!("{e}"));
        let expected: Vec<_> = workspace.projects.iter().map(import_project).collect();
        let s =
            import_source(vec![workspace], &CancelFlag::new()).unwrap_or_else(|e| panic!("{e}"));
        assert!(!s.truncated, "{version}");
        assert!(s.issues.is_empty(), "{version}: {:?}", s.issues);
        assert_eq!(s.projects.len(), expected.len(), "{version}");
        for (got, want) in s.projects.iter().zip(&expected) {
            assert_eq!(got.import.connections.len(), want.connections.len());
            assert_eq!(got.import.issues, want.issues, "{version}");
            for (g, w) in got.import.connections.iter().zip(&want.connections) {
                assert_eq!(g.dbeaver_id, w.dbeaver_id);
                assert_eq!(g.issues, w.issues, "{version}");
                assert_eq!(g.status(), w.status());
            }
        }
    }
}
