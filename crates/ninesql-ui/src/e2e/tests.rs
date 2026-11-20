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
fn probe_keys_are_unique_and_documented() {
    let mut seen = std::collections::BTreeSet::new();
    for k in ProbeKey::ALL {
        assert!(seen.insert(k.name()), "{}", k.name());
        assert!(!k.doc().is_empty());
        assert_eq!(ProbeKey::parse(k.name()), Some(*k));
    }
    assert!(ProbeKey::parse("editor.password").is_none());
}

#[gpui::test]
fn sqlite_editor_grid_runs_headless(cx: &mut gpui::TestAppContext) {
    let Some((report, root)) = super::headless::run_scenario(cx, "sqlite-editor-grid") else {
        return;
    };
    super::headless::assert_passed(&report);
    let (scanned, hits) = super::setup::scan_for_secrets(&root, &["nine_dev_only"], &[]);
    assert!(scanned > 0 && hits.is_empty());
}
