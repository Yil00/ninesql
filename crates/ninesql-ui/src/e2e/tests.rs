//! Scenario validation without a display (`cargo test -p ninesql-ui --features e2e-scenarios
//! e2e::`): every file of `e2e/scenarios/` parses, names registered actions and known probe
//! keys only, and is runnable alone.

use std::path::PathBuf;

use super::probe::ProbeKey;
use super::scenario::{Scenario, StepKind};

use super::headless::scenarios_dir;

fn scenario_files() -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(scenarios_dir())
        .unwrap_or_else(|e| panic!("e2e/scenarios: {e}"))
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "toml"))
        .collect();
    files.sort();
    files
}

#[test]
fn every_scenario_file_parses_with_registered_actions_and_known_probe_keys() {
    let files = scenario_files();
    assert!(!files.is_empty(), "no scenario in e2e/scenarios");
    let mut names = std::collections::BTreeSet::new();
    for file in &files {
        let s = Scenario::load(file).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
        assert!(
            names.insert(s.name.clone()),
            "duplicate scenario name {}",
            s.name
        );
        assert_eq!(
            file.file_stem().and_then(|n| n.to_str()),
            Some(s.name.as_str()),
            "the file is named after its scenario"
        );
        assert!(!s.description.is_empty(), "{}: a description", s.name);
        assert!(
            !s.checkpoints().is_empty(),
            "{}: at least one checkpoint",
            s.name
        );
        for step in &s.steps {
            match &step.kind {
                StepKind::Action(a) => {
                    assert!(crate::actions::registry().iter().any(|x| x.name() == a));
                }
                StepKind::Wait(c, _) | StepKind::Expect(c) => {
                    assert_eq!(ProbeKey::parse(c.key.name()), Some(c.key));
                }
                _ => {}
            }
        }
    }
}

#[test]
fn invalid_scenarios_are_refused_whole() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let base = scenarios_dir();
    let parse = |body: &str| {
        Scenario::parse(
            &format!("schema_version = 1\nname = \"t\"\n{body}"),
            &base,
            &root,
        )
    };
    let ok = "[[step]]\ncheckpoint = \"c\"\n";
    parse(ok).unwrap_or_else(|e| panic!("{e}"));
    for (body, why) in [
        (
            "[[step]]\naction = \"ninesql::NoSuchAction\"\n",
            "not a registered action",
        ),
        (
            "[[step]]\naction = \"NewEditorTab\"\n",
            "not a registered action",
        ),
        (
            "[[step]]\nwait = \"editor.password\"\nequals = true\n",
            "not a probe key",
        ),
        (
            "[[step]]\nwait = \"grid.rows\"\nequals = true\n",
            "does not apply",
        ),
        (
            "[[step]]\nexpect = \"session.open\"\nequals = true\n",
            "needs an arg",
        ),
        (
            "[[step]]\nwait = \"grid.rows\"\nat_least = 1\ntimeout_secs = 500\n",
            "1 to 120",
        ),
        ("[[step]]\nkeys = \"hyper-x\"\n", "unknown"),
        (
            "[[step]]\ncheckpoint = \"c\"\naction = \"ninesql::Quit\"\n",
            "exactly one",
        ),
        ("[[step]]\ncheckpoint = \"Bad Name\"\n", "checkpoint name"),
        (
            "[[step]]\ncheckpoint = \"c\"\n[[step]]\ncheckpoint = \"c\"\n",
            "distinct",
        ),
        ("[[step]]\ntext = \"a\\u0007b\"\n", "control"),
        (
            "engines = \"/etc/hostname\"\n[[step]]\ncheckpoint = \"c\"\n",
            "inside the repository",
        ),
        (
            "[[connection]]\nname = \"x\"\nengine = \"oracle\"\n[[step]]\ncheckpoint = \"c\"\n",
            "engine must be",
        ),
        (
            "[[connection]]\nname = \"x\"\nengine = \"postgresql\"\n[[step]]\ncheckpoint = \"c\"\n",
            "needs `engines`",
        ),
        (
            "[[connection]]\nname = \"x\"\nengine = \"sqlite\"\nfile = \"../x.db\"\n[[step]]\ncheckpoint = \"c\"\n",
            "plain `file`",
        ),
        (
            "[[dbeaver]]\nfixture = \"../../src\"\n[[step]]\ncheckpoint = \"c\"\n",
            "directory only",
        ),
        (
            "[[step]]\ncheckpoint = \"c\"\nunknown_key = 1\n",
            "not valid",
        ),
        (
            "timeout_secs = 0\n[[step]]\ncheckpoint = \"c\"\n",
            "timeout_secs",
        ),
    ] {
        let e = parse(body)
            .err()
            .unwrap_or_else(|| panic!("accepted: {body}"));
        assert!(e.0.contains(why), "{body}: {e}");
    }
    // The file's own name and schema.
    assert!(
        Scenario::parse(
            "schema_version = 2\nname = \"t\"\n[[step]]\ncheckpoint = \"c\"\n",
            &base,
            &root
        )
        .is_err()
    );
}

#[test]
fn probe_keys_are_unique_and_documented() {
    let mut seen = std::collections::BTreeSet::new();
    for k in ProbeKey::ALL {
        assert!(seen.insert(k.name()), "{}", k.name());
        assert!(!k.doc().is_empty());
        assert_eq!(ProbeKey::parse(k.name()), Some(*k));
    }
    assert!(ProbeKey::parse("editor.password").is_none());
}

/// Runs a scenario headless and checks every step passed and the I1 scan of its directory.
fn passes_headless(cx: &mut gpui::TestAppContext, name: &str) {
    let Some((report, root)) = super::headless::run_scenario(cx, name) else {
        return;
    };
    super::headless::assert_passed(&report);
    let engines = super::setup::DevEngines::load(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../dev/engines.json"),
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let secrets: Vec<&str> = engines.passwords().iter().map(String::as_str).collect();
    let (scanned, hits) = super::setup::scan_for_secrets(&root, &secrets, &[]);
    assert!(scanned > 0, "the scan found the run's files");
    assert!(hits.is_empty(), "I1: a dev password is in {hits:?}");
}

macro_rules! headless {
    ($($test:ident => $name:literal;)*) => {
        $(
            #[gpui::test]
            fn $test(cx: &mut gpui::TestAppContext) {
                passes_headless(cx, $name);
            }
        )*
    };
}

// Not run headless: `dbeaver-import`. The DBeaver detection reads the process's `HOME`, which is
// the run's own only in a real scenario run; a headless test must never look at the developer's
// own DBeaver workspaces (I4). It is checked by the parse test above.
headless! {
    sqlite_editor_grid_runs_headless => "sqlite-editor-grid";
    sqlite_navigator_object_runs_headless => "sqlite-navigator-object";
    sqlite_table_edit_runs_headless => "sqlite-table-edit";
    sqlite_safety_runs_headless => "sqlite-safety";
    sqlite_transfer_runs_headless => "sqlite-transfer";
    postgresql_editor_safety_runs_headless => "postgresql-editor-safety";
    mysql_editor_safety_runs_headless => "mysql-editor-safety";
    mariadb_editor_safety_runs_headless => "mariadb-editor-safety";
}
