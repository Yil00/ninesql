//! The step runner (ADR-0029 §2.6, §2.7): one task on the GPUI foreground executor that waits
//! with GPUI timers, never a blocking wait (I6). It first saves the scenario's connections
//! through the hub (as the connection dialog's "Save" does, secrets into the run's vault), then
//! runs the steps in order:
//!
//! - `action`: the registered action is dispatched to the focused element, as its key binding
//!   would be; when the focused element does not handle it and no dialog is open, it goes the
//!   command palette's way ([`Workspace::perform`]). Which path was taken is recorded.
//! - `keys` / `text`: keystrokes through GPUI's own keystroke dispatch to the focused element
//!   (`Window::dispatch_keystroke`, the path a real key press takes after the platform layer).
//! - `wait`: the probe is read every 50 ms until it matches or the step times out.
//! - `checkpoint`: once the step's state has been presented in a frame, one line
//!   `ninesql-e2e checkpoint <n> <name>` goes to standard output and the UI is held still for
//!   the scenario's `checkpoint_hold_ms` (the harness captures the window meanwhile).
//! - `expect`: the probe is read once, recorded pass or fail; the run goes on.
//!
//! A failed or timed-out step (other than `expect`) ends the run: the following steps are
//! recorded `skipped`. Nothing typed is recorded: step results carry the step's name, kind,
//! outcome, duration, the checkpoint name, and failure messages built from the probe (texts only
//! for [`super::probe::ProbeKey::recordable`] keys).

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui_kit::component::WindowExt as _;
use gpui_kit::{AnyWindowHandle, App, AsyncApp, Entity, Keystroke, Task};

use super::probe::{self, ProbeValue};
use super::scenario::{Condition, Scenario, Step, StepKind};
use super::setup::{self, DevEngines};
use crate::connections::hub::Hub;
use crate::workspace::Workspace;

/// How often a `wait` reads the probe.
const POLL: Duration = Duration::from_millis(50);
/// Pause after each step, so its effects are drawn before the next one.
const SETTLE: Duration = Duration::from_millis(60);
/// Longest wait for the core to start and for each connection to be saved.
const SETUP_TIMEOUT: Duration = Duration::from_secs(90);
/// Longest wait for a frame at a checkpoint.
const FRAME_TIMEOUT: Duration = Duration::from_secs(5);

/// The outcome of one step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Done, or the condition held.
    Pass,
    /// The action could not be dispatched, or an `expect` did not hold.
    Fail,
    /// A `wait` (or the whole run) ran out of time.
    Timeout,
    /// Not run (an earlier step failed, or the run was skipped).
    Skipped,
}

impl Outcome {
    /// The name in `results.json`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Timeout => "timeout",
            Self::Skipped => "skipped",
        }
    }
}

/// The result of one step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepResult {
    /// 1-based.
    pub index: usize,
    /// The step's name.
    pub name: String,
    /// `action`, `keys`, `text`, `wait`, `checkpoint`, `expect`.
    pub kind: &'static str,
    /// What happened.
    pub outcome: Outcome,
    /// How long it took.
    pub duration: Duration,
    /// The checkpoint's name.
    pub checkpoint: Option<String>,
    /// Why it failed, timed out or was skipped (built from the probe, no typed text).
    pub message: Option<String>,
    /// `focus` or `palette`, for an `action`.
    pub dispatched: Option<&'static str>,
}

/// Size and scale of the window, for `results.json`.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct WindowFacts {
    /// Logical width.
    pub width: f32,
    /// Logical height.
    pub height: f32,
    /// Scale factor.
    pub scale_factor: f32,
}

/// What a run reports.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RunReport {
    /// Why the setup failed (the core did not start, a connection was not saved).
    pub setup_error: Option<String>,
    /// Per step.
    pub steps: Vec<StepResult>,
    /// The window.
    pub window: WindowFacts,
    /// Checkpoint names reached, in order.
    pub checkpoints: Vec<String>,
}

impl RunReport {
    /// Every step passed and the setup worked.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.setup_error.is_none() && self.steps.iter().all(|s| s.outcome == Outcome::Pass)
    }
}

/// How a run is driven.
pub struct RunnerConfig {
    /// The scenario.
    pub scenario: Scenario,
    /// `dev/engines.json`, when the scenario names dev engines.
    pub engines: Option<DevEngines>,
    /// The run's HOME (`{home}` in `text` steps, SQLite files).
    pub home: PathBuf,
    /// The window and its workspace.
    pub window: AnyWindowHandle,
    /// The workspace.
    pub workspace: Entity<Workspace>,
    /// Wait for a presented frame at checkpoints (off in headless tests: no frame loop).
    pub wait_for_frame: bool,
    /// Where checkpoint lines go (standard output in the binary).
    pub checkpoint_sink: Box<dyn FnMut(&str)>,
}

/// Starts the run on the foreground executor.
pub fn spawn(config: RunnerConfig, cx: &mut App) -> Task<RunReport> {
    cx.spawn(async move |cx| run(config, cx).await)
}

async fn sleep(cx: &AsyncApp, d: Duration) {
    cx.background_executor().timer(d).await;
}

async fn run(mut config: RunnerConfig, cx: &mut AsyncApp) -> RunReport {
    let mut report = RunReport::default();
    if let Err(e) = save_connections(&config, cx).await {
        report.setup_error = Some(e);
        report.steps = config
            .scenario
            .steps
            .iter()
            .enumerate()
            .map(|(i, s)| skipped(i + 1, s, "the setup failed"))
            .collect();
        return report;
    }
    let deadline = Instant::now() + config.scenario.timeout;
    let steps = config.scenario.steps.clone();
    let mut stop: Option<&'static str> = None;
    let mut checkpoint_no = 0;
    for (i, step) in steps.iter().enumerate() {
        let index = i + 1;
        if let Some(why) = stop {
            report.steps.push(skipped(index, step, why));
            continue;
        }
        if Instant::now() >= deadline {
            report.steps.push(StepResult {
                outcome: Outcome::Timeout,
                message: Some("the whole-run timeout was reached".to_owned()),
                ..skipped(index, step, "")
            });
            stop = Some("the whole-run timeout was reached");
            continue;
        }
        let started = Instant::now();
        let mut result = StepResult {
            index,
            name: step.name.clone(),
            kind: step.kind.kind_name(),
            outcome: Outcome::Pass,
            duration: Duration::ZERO,
            checkpoint: None,
            message: None,
            dispatched: None,
        };
        match &step.kind {
            StepKind::Action(name) => match dispatch_action(name, &config, cx) {
                Ok(path) => result.dispatched = Some(path),
                Err(e) => fail(&mut result, e),
            },
            StepKind::Keys(strokes) => {
                for stroke in strokes {
                    if let Err(e) = keystroke(stroke, &config, cx) {
                        fail(&mut result, e);
                        break;
                    }
                    sleep(cx, Duration::from_millis(30)).await;
                }
            }
            StepKind::Text(text) => {
                let text = text.replace("{home}", &config.home.to_string_lossy());
                if let Err(e) = type_text(&text, &config, cx) {
                    fail(&mut result, e);
                }
            }
            StepKind::Wait(condition, timeout) => {
                let until = (Instant::now() + *timeout).min(deadline);
                loop {
                    let value = read(condition, &config, cx);
                    if probe::matches(&value, &condition.matchers) {
                        break;
                    }
                    if Instant::now() >= until {
                        result.outcome = Outcome::Timeout;
                        result.message = Some(mismatch(condition, &value));
                        break;
                    }
                    sleep(cx, POLL).await;
                }
            }
            StepKind::Expect(condition) => {
                let value = read(condition, &config, cx);
                if !probe::matches(&value, &condition.matchers) {
                    fail(&mut result, mismatch(condition, &value));
                }
            }
            StepKind::Checkpoint(name) => {
                checkpoint_no += 1;
                if config.wait_for_frame && !presented(&config, cx).await {
                    tracing::warn!(checkpoint = %name, "no frame presented before the checkpoint");
                }
                (config.checkpoint_sink)(&format!("ninesql-e2e checkpoint {checkpoint_no} {name}"));
                sleep(cx, config.scenario.checkpoint_hold).await;
                result.checkpoint = Some(name.clone());
                report.checkpoints.push(name.clone());
            }
        }
        if !matches!(step.kind, StepKind::Checkpoint(_)) {
            sleep(cx, SETTLE).await;
        }
        result.duration = started.elapsed();
        if matches!(result.outcome, Outcome::Fail | Outcome::Timeout)
            && !matches!(step.kind, StepKind::Expect(_))
        {
            stop = Some("an earlier step failed");
        }
        report.steps.push(result);
    }
    report.window = config
        .window
        .update(cx, |_, window, _| {
            let size = window.bounds().size;
            WindowFacts {
                width: f32::from(size.width),
                height: f32::from(size.height),
                scale_factor: window.scale_factor(),
            }
        })
        .unwrap_or_default();
    report
}

fn skipped(index: usize, step: &Step, why: &str) -> StepResult {
    StepResult {
        index,
        name: step.name.clone(),
        kind: step.kind.kind_name(),
        outcome: Outcome::Skipped,
        duration: Duration::ZERO,
        checkpoint: None,
        message: (!why.is_empty()).then(|| why.to_owned()),
        dispatched: None,
    }
}

fn fail(result: &mut StepResult, message: impl Into<String>) {
    result.outcome = Outcome::Fail;
    result.message = Some(message.into());
}

fn mismatch(condition: &Condition, value: &ProbeValue) -> String {
    let record = condition.key.recordable();
    let wanted: Vec<String> = condition
        .matchers
        .iter()
        .map(|m| m.describe(record))
        .collect();
    format!(
        "{} is {}, expected {}",
        condition.key.name(),
        probe::describe(condition.key, value),
        wanted.join(" and ")
    )
}

/// Waits until the core has started, then saves each connection of the scenario.
async fn save_connections(config: &RunnerConfig, cx: &mut AsyncApp) -> Result<(), String> {
    let started = Instant::now();
    let hub = loop {
        let ready = cx.update(|cx| {
            Hub::get(cx).filter(|h| {
                let h = h.read(cx);
                !h.is_starting() && h.manager().is_some()
            })
        });
        if let Some(h) = ready {
            break h;
        }
        let failed = cx.update(|cx| {
            Hub::get(cx).and_then(|h| {
                let h = h.read(cx);
                (!h.is_starting() && h.manager().is_none())
                    .then(|| h.problems().into_iter().next().unwrap_or_default())
            })
        });
        if let Some(problem) = failed {
            return Err(format!("the core did not start: {problem}"));
        }
        if started.elapsed() > SETUP_TIMEOUT {
            return Err("the core did not start in time".to_owned());
        }
        sleep(cx, POLL).await;
    };
    for c in &config.scenario.connections {
        let built = setup::built_connection(c, config.engines.as_ref(), &config.home)?;
        let task = hub.update(cx, |h, cx| h.save(built, cx));
        task.await
            .map_err(|e| format!("the connection \"{}\" was not saved: {e}", c.name))?;
    }
    Ok(())
}

fn dispatch_action(
    name: &str,
    config: &RunnerConfig,
    cx: &mut AsyncApp,
) -> Result<&'static str, String> {
    let spec = crate::actions::registry()
        .iter()
        .find(|s| s.name() == name)
        .ok_or_else(|| format!("{name} is not registered"))?;
    let ws = config.workspace.clone();
    config
        .window
        .update(cx, |_, window, cx| {
            let action = (spec.build)();
            if window.is_action_available(action.as_ref(), cx) {
                window.dispatch_action(action, cx);
                Ok("focus")
            } else if window.has_active_dialog(cx) {
                Err(format!(
                    "{name} is not available where the focus is (a dialog is open)"
                ))
            } else if ws.update(cx, |ws, cx| ws.perform(action.as_ref(), window, cx)) {
                Ok("palette")
            } else {
                Err(format!("{name} is not handled"))
            }
        })
        .map_err(|_| "the window is closed".to_owned())?
}

fn keystroke(stroke: &str, config: &RunnerConfig, cx: &mut AsyncApp) -> Result<(), String> {
    config
        .window
        .update(cx, |_, window, cx| {
            let os = cx.platform_info().os;
            let ks = Keystroke::parse_with_os(stroke, os)
                .map_err(|_| format!("`{stroke}` is not a keystroke"))?;
            window.dispatch_keystroke(ks, cx);
            Ok(())
        })
        .map_err(|_| "the window is closed".to_owned())?
}

fn type_text(text: &str, config: &RunnerConfig, cx: &mut AsyncApp) -> Result<(), String> {
    config
        .window
        .update(cx, |_, window, cx| {
            let os = cx.platform_info().os;
            for c in text.chars() {
                let ks = match c {
                    '\n' => Keystroke::parse_with_os("enter", os).ok(),
                    ' ' => Keystroke::parse_with_os("space", os).ok(),
                    '\t' => Keystroke::parse_with_os("tab", os).ok(),
                    c => Some(Keystroke {
                        modifiers: gpui_kit::Modifiers::default(),
                        key: c.to_string(),
                        key_char: Some(c.to_string()),
                    }),
                };
                if let Some(ks) = ks {
                    window.dispatch_keystroke(ks, cx);
                }
            }
        })
        .map_err(|_| "the window is closed".to_owned())
}

fn read(condition: &Condition, config: &RunnerConfig, cx: &mut AsyncApp) -> ProbeValue {
    let ws = config.workspace.clone();
    config
        .window
        .update(cx, |_, window, cx| {
            probe::read(condition.key, condition.arg.as_deref(), &ws, window, cx)
        })
        .unwrap_or(ProbeValue::Absent)
}

/// Waits until a frame drawn after this call has been presented.
async fn presented(config: &RunnerConfig, cx: &mut AsyncApp) -> bool {
    let flag = Rc::new(Cell::new(false));
    let set = flag.clone();
    let scheduled = config
        .window
        .update(cx, |_, window, _| {
            window.refresh();
            window.on_next_frame(move |_, _| set.set(true));
        })
        .is_ok();
    if !scheduled {
        return false;
    }
    let started = Instant::now();
    while !flag.get() {
        if started.elapsed() > FRAME_TIMEOUT {
            return false;
        }
        sleep(cx, Duration::from_millis(16)).await;
    }
    true
}
