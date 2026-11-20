//! A scenario run in a real window: [`prepare`] (before GPUI and before the subscriber, which
//! reads the run's settings) and [`Prepared::run`].

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use gpui_kit::AppContext as _;
use ninesql_core::secrets::SecretString;
use zeroize::Zeroizing;

use super::ExitStatus;
use super::runner::{self, RunReport};
use super::scenario::{Scenario, repository_root};
use super::setup::{self, DevEngines};

/// A run that passed the checks of ADR-0029 §2.4 and §2.5 and whose files are written.
pub struct Prepared {
    scenario: Scenario,
    engines: Option<DevEngines>,
    run_dir: PathBuf,
    out_dir: PathBuf,
    home: PathBuf,
}

/// Why [`prepare`] stopped: the status, and a fixed message for standard error.
#[derive(Debug)]
pub struct Stopped {
    /// [`ExitStatus::Refused`], [`ExitStatus::Skipped`] or [`ExitStatus::SetupError`].
    pub status: ExitStatus,
    /// What to print (no secret, no typed text).
    pub message: String,
}

fn refused(message: impl Into<String>) -> Stopped {
    Stopped {
        status: ExitStatus::Refused,
        message: message.into(),
    }
}

/// Steps 1 to 4 of the module documentation. **Blocking** (file checks, the engines' TCP
/// probes, the SQLite seed): call it from `main`, before GPUI starts.
///
/// # Errors
/// [`Stopped`].
pub fn prepare(scenario_file: &Path, out_dir: &Path) -> Result<Prepared, Stopped> {
    let isolated = super::isolation::check(
        |k| std::env::var_os(k),
        out_dir,
        super::isolation::account_home().as_deref(),
        &std::env::temp_dir(),
    )
    .map_err(refused)?;
    let scenario = Scenario::load(scenario_file).map_err(|e| refused(e.0))?;
    ninesql_core::e2e::install(&isolated.run_dir).map_err(|e| refused(e.to_string()))?;
    let engines = scenario
        .engines
        .as_deref()
        .map(DevEngines::load)
        .transpose()
        .map_err(refused)?;
    let prepared = Prepared {
        scenario,
        engines,
        run_dir: isolated.run_dir,
        out_dir: isolated.out_dir,
        home: isolated.home,
    };
    for id in prepared.scenario.dev_engines() {
        let up = prepared
            .engines
            .as_ref()
            .and_then(|e| e.get(id))
            .is_some_and(setup::reachable);
        if !up {
            let message = format!(
                "the dev engine {id} does not answer (python3 dev/ready.py up core); the scenario is skipped"
            );
            prepared.write_results(
                &RunReport::default(),
                ExitStatus::Skipped,
                Some(&message),
                None,
            );
            return Err(Stopped {
                status: ExitStatus::Skipped,
                message,
            });
        }
    }
    let repo = repository_root(scenario_file)
        .or_else(|| {
            std::fs::canonicalize(scenario_file)
                .ok()
                .as_deref()
                .and_then(repository_root)
        })
        .ok_or_else(|| refused("the scenario file is not inside the NineSQL repository"))?;
    if let Err(message) = setup::prepare_home(&prepared.home, &prepared.scenario, &repo) {
        prepared.write_results(
            &RunReport::default(),
            ExitStatus::SetupError,
            Some(&message),
            None,
        );
        return Err(Stopped {
            status: ExitStatus::SetupError,
            message,
        });
    }
    Ok(prepared)
}

/// The commit of the run, from `NINESQL_E2E_COMMIT` (set by the harness; only read, never a
/// trigger), hexadecimal and at most 40 characters, else `unknown`.
fn commit() -> String {
    std::env::var("NINESQL_E2E_COMMIT")
        .ok()
        .filter(|c| !c.is_empty() && c.len() <= 40 && c.bytes().all(|b| b.is_ascii_hexdigit()))
        .unwrap_or_else(|| "unknown".to_owned())
}

/// The I1 scan's result for `results.json`.
pub struct ScanResult {
    files_scanned: usize,
    hits: Vec<String>,
}

impl Prepared {
    /// The run directory.
    #[must_use]
    pub fn run_dir(&self) -> &Path {
        &self.run_dir
    }

    /// Writes `results.json` (atomic, `0600`). Holds no statement text, parameter value, cell
    /// value or password (ADR-0029 §2.8).
    fn write_results(
        &self,
        report: &RunReport,
        status: ExitStatus,
        message: Option<&str>,
        scan: Option<&ScanResult>,
    ) {
        let doc = results_json(&self.scenario, report, status, message, scan);
        let text = serde_json::to_vec_pretty(&doc).unwrap_or_default();
        if let Err(e) =
            ninesql_core::storage::fs::write_atomic(&self.out_dir.join("results.json"), &text)
        {
            tracing::error!(error = %e, "results.json was not written");
        }
    }

    /// Steps 5 and 6: the application in a real window, the steps, `results.json`, the I1 scan.
    /// Returns when the application has quit.
    pub fn run(self, registry: ninesql_core::DriverRegistry) -> ExitStatus {
        let status = Arc::new(AtomicU8::new(ExitStatus::SetupError.code()));
        let master = setup::master_password();
        let settings = crate::settings::load_from_env();
        let app = gpui_kit::application().with_assets(gpui_kit::assets::Assets);
        let out = Arc::clone(&status);
        app.run(move |cx| {
            crate::app::init(settings, cx);
            crate::connections::hub::start_for_scenario(
                registry,
                SecretString::from(master.as_str().to_owned()),
                cx,
            );
            crate::scripts::install_quit_handler(cx);
            cx.on_app_quit(|cx| {
                if let Some(hub) = crate::connections::hub::Hub::get(cx) {
                    hub.update(cx, |hub, _| hub.shutdown(std::time::Duration::from_secs(2)));
                }
                async {}
            })
            .detach();
            let Some((window, workspace)) = crate::app::open_window(cx) else {
                out.store(ExitStatus::SetupError.code(), Ordering::SeqCst);
                cx.quit();
                return;
            };
            cx.activate(true);
            let config = runner::RunnerConfig {
                scenario: self.scenario.clone(),
                engines: self.engines.clone(),
                home: self.home.clone(),
                window,
                workspace,
                wait_for_frame: true,
                checkpoint_sink: Box::new(|line| {
                    use std::io::Write as _;
                    let mut stdout = std::io::stdout().lock();
                    let _ = writeln!(stdout, "{line}");
                    let _ = stdout.flush();
                }),
            };
            let task = runner::spawn(config, cx);
            cx.spawn(async move |cx| {
                let report = task.await;
                let finish = cx.background_spawn(async move {
                    let scan = self.scan(&master);
                    let status = if !scan.hits.is_empty() {
                        ExitStatus::SecretFound
                    } else if report.setup_error.is_some() {
                        ExitStatus::SetupError
                    } else if report.passed() {
                        ExitStatus::Pass
                    } else {
                        ExitStatus::Fail
                    };
                    self.write_results(&report, status, report.setup_error.as_deref(), Some(&scan));
                    // `results.json` was just written: scan it too.
                    let again = self.scan(&master);
                    let status = if again.hits.is_empty() {
                        status
                    } else {
                        ExitStatus::SecretFound
                    };
                    drop(master);
                    status
                });
                let status = finish.await;
                out.store(status.code(), Ordering::SeqCst);
                cx.update(|cx| cx.quit());
            })
            .detach();
        });
        match status.load(Ordering::SeqCst) {
            0 => ExitStatus::Pass,
            1 => ExitStatus::Fail,
            3 => ExitStatus::SecretFound,
            5 => ExitStatus::Skipped,
            _ => ExitStatus::SetupError,
        }
    }

    /// I1 (ADR-0029 §2.10): every file under the run directory but the vault, for each password
    /// of `dev/engines.json` and the master password.
    fn scan(&self, master: &Zeroizing<String>) -> ScanResult {
        let mut secrets: Vec<&str> = self
            .engines
            .as_ref()
            .map(|e| e.passwords().iter().map(String::as_str).collect())
            .unwrap_or_default();
        secrets.push(master.as_str());
        let vault = ninesql_core::storage::Paths::from_env()
            .map(|p| vec![p.vault_file()])
            .unwrap_or_default();
        let (files_scanned, hits) = setup::scan_for_secrets(&self.run_dir, &secrets, &vault);
        ScanResult {
            files_scanned,
            hits,
        }
    }
}

/// `results.json` (schema [`super::RESULTS_SCHEMA_VERSION`]).
#[must_use]
pub fn results_json(
    scenario: &Scenario,
    report: &RunReport,
    status: ExitStatus,
    message: Option<&str>,
    scan: Option<&ScanResult>,
) -> serde_json::Value {
    let steps: Vec<serde_json::Value> = if report.steps.is_empty() {
        scenario
            .steps
            .iter()
            .enumerate()
            .map(|(i, s)| {
                serde_json::json!({
                    "index": i + 1,
                    "name": s.name,
                    "kind": s.kind.kind_name(),
                    "outcome": "skipped",
                    "duration_ms": 0,
                    "checkpoint": serde_json::Value::Null,
                    "message": message.unwrap_or("not run"),
                })
            })
            .collect()
    } else {
        report
            .steps
            .iter()
            .map(|s| {
                serde_json::json!({
                    "index": s.index,
                    "name": s.name,
                    "kind": s.kind,
                    "outcome": s.outcome.name(),
                    "duration_ms": u64::try_from(s.duration.as_millis()).unwrap_or(u64::MAX),
                    "checkpoint": s.checkpoint,
                    "message": s.message,
                    "dispatched": s.dispatched,
                })
            })
            .collect()
    };
    serde_json::json!({
        "schema_version": super::RESULTS_SCHEMA_VERSION,
        "commit": commit(),
        "scenario": scenario.name,
        "build": "e2e-scenarios test build, not for distribution",
        "outcome": status.name(),
        "exit_code": status.code(),
        "message": message,
        "window": {
            "width": report.window.width,
            "height": report.window.height,
            "scale_factor": report.window.scale_factor,
        },
        "checkpoints": report.checkpoints,
        "planned_checkpoints": scenario.checkpoints(),
        "steps": steps,
        "i1_scan": scan.map(|s| serde_json::json!({
            "outcome": if s.hits.is_empty() { "pass" } else { "fail" },
            "files_scanned": s.files_scanned,
            "files_with_a_secret": s.hits,
        })),
    })
}
