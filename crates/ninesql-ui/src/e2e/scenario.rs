//! Scenario files (ADR-0029 §2.6): TOML, the format of `settings.toml`, reviewed like source
//! code. A scenario names the dev engines it connects to, the files it needs in the run
//! directory, and a list of steps; it is parsed and checked whole before anything runs.
//!
//! ```toml
//! schema_version = 1
//! name = "connect-postgresql"
//! description = "Connect to the dev PostgreSQL from the navigator."
//! engines = "../../dev/engines.json"      # relative to this file, inside the repository
//! timeout_secs = 900                       # whole run (default 900, at most 3600)
//! checkpoint_hold_ms = 1500                # default 1500
//!
//! [[connection]]                           # saved before the window opens
//! name = "Dev PostgreSQL"
//! engine = "postgresql"                    # a dev engine id of engines.json, or "sqlite"
//! type = "dev"                             # "dev", "test" or "prod"
//! read_only = false
//! # file = "nine.db"                       # sqlite: created in the run's HOME from the seed
//!
//! [[dbeaver]]                              # a synthetic fixture copied under the run's HOME
//! fixture = "dbeaver-25.3.5"
//!
//! [[file]]                                 # a small file written in the run's HOME
//! name = "rows.csv"
//! content = "id,label\n1,one\n"
//!
//! [[step]]
//! action = "ninesql::FocusNavigator"       # a registered action, by its full name
//! [[step]]
//! keys = "home"                            # keystrokes, GPUI syntax, space separated
//! [[step]]
//! text = "SELECT 1"                        # typed into the focused element; `{home}` expands
//! [[step]]
//! wait = "session.open"                    # a probe key (probe.rs)
//! arg = "Dev PostgreSQL"
//! equals = true
//! timeout_secs = 30                        # default 10, at most 120
//! [[step]]
//! checkpoint = "connected"                 # a screenshot point
//! [[step]]
//! expect = "grid.rows"
//! at_least = 1
//! ```

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use super::probe::{ProbeKey, ValueKind};

/// `schema_version` this build reads.
pub const SCHEMA_VERSION: i64 = 1;
/// Largest scenario file read.
pub const MAX_SCENARIO_BYTES: u64 = 1024 * 1024;
/// Default timeout of a `wait` step.
pub const DEFAULT_STEP_TIMEOUT: Duration = Duration::from_secs(10);
/// Longest timeout of a `wait` step.
pub const MAX_STEP_TIMEOUT: Duration = Duration::from_secs(120);
/// Default whole-run timeout.
pub const DEFAULT_RUN_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// Longest whole-run timeout.
pub const MAX_RUN_TIMEOUT: Duration = Duration::from_secs(60 * 60);
/// Default hold at a checkpoint (ADR-0029 §2.7, open question 5).
pub const DEFAULT_CHECKPOINT_HOLD: Duration = Duration::from_millis(1500);
/// Most steps in one scenario.
pub const MAX_STEPS: usize = 2000;
/// Largest `[[file]]` content.
pub const MAX_FILE_BYTES: usize = 1024 * 1024;

/// The engines a scenario may name besides the dev engines of `engines.json`.
pub const SQLITE: &str = "sqlite";
/// Dev engines a scenario may connect to (ADR-0029 §2.5).
pub const DEV_ENGINES: &[&str] = &["postgresql", "mysql", "mariadb"];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawScenario {
    schema_version: i64,
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    engines: Option<String>,
    #[serde(default)]
    timeout_secs: Option<u64>,
    #[serde(default)]
    checkpoint_hold_ms: Option<u64>,
    #[serde(default)]
    connection: Vec<RawConnection>,
    #[serde(default)]
    dbeaver: Vec<RawFixture>,
    #[serde(default)]
    file: Vec<RawFile>,
    #[serde(default)]
    step: Vec<RawStep>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConnection {
    name: String,
    engine: String,
    #[serde(default, rename = "type")]
    ty: Option<String>,
    #[serde(default)]
    read_only: bool,
    #[serde(default)]
    file: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFixture {
    fixture: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    name: String,
    content: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStep {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    keys: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    wait: Option<String>,
    #[serde(default)]
    checkpoint: Option<String>,
    #[serde(default)]
    expect: Option<String>,
    #[serde(default)]
    arg: Option<String>,
    #[serde(default)]
    equals: Option<toml::Value>,
    #[serde(default)]
    at_least: Option<i64>,
    #[serde(default)]
    at_most: Option<i64>,
    #[serde(default)]
    contains: Option<String>,
    #[serde(default)]
    timeout_secs: Option<u64>,
}

/// The connection type of a scenario connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionKind {
    /// "dev".
    Dev,
    /// "test".
    Test,
    /// "prod" (production: colored frame and confirmations).
    Production,
}

/// A connection the runner saves before the window opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScenarioConnection {
    /// Display name (the navigator label, the probe argument).
    pub name: String,
    /// `postgresql`, `mysql`, `mariadb` or `sqlite`.
    pub engine: String,
    /// Its type.
    pub kind: ConnectionKind,
    /// Read-only session (enforced by the driver, I3).
    pub read_only: bool,
    /// SQLite: the file name in the run's HOME, created from `dev/seed/sqlite/nine.sql`.
    pub file: Option<String>,
}

/// A small file the runner writes in the run's HOME.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScenarioFile {
    /// A plain file name (no directory).
    pub name: String,
    /// Its text.
    pub content: String,
}

/// What a `wait` or an `expect` compares with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Matcher {
    /// Equal to a boolean.
    Bool(bool),
    /// Equal to an integer.
    Int(i64),
    /// Equal to a text.
    Text(String),
    /// At least.
    AtLeast(i64),
    /// At most.
    AtMost(i64),
    /// Contains a text.
    Contains(String),
}

impl Matcher {
    fn fits(&self, kind: ValueKind) -> bool {
        match self {
            Self::Bool(_) => kind == ValueKind::Bool,
            Self::Int(_) | Self::AtLeast(_) | Self::AtMost(_) => kind == ValueKind::Int,
            Self::Text(_) | Self::Contains(_) => kind == ValueKind::Text,
        }
    }

    /// A short description for `results.json`, with no value when `record_text` is false.
    #[must_use]
    pub fn describe(&self, record_text: bool) -> String {
        match self {
            Self::Bool(b) => format!("equals {b}"),
            Self::Int(n) => format!("equals {n}"),
            Self::AtLeast(n) => format!("at least {n}"),
            Self::AtMost(n) => format!("at most {n}"),
            Self::Text(t) if record_text => format!("equals {t:?}"),
            Self::Contains(t) if record_text => format!("contains {t:?}"),
            Self::Text(_) => "equals a text".to_owned(),
            Self::Contains(_) => "contains a text".to_owned(),
        }
    }
}

/// One condition on the probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Condition {
    /// The probe key.
    pub key: ProbeKey,
    /// Its argument (a connection name, a row label), when the key takes one.
    pub arg: Option<String>,
    /// The matchers, all of which must hold.
    pub matchers: Vec<Matcher>,
}

/// What a step does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepKind {
    /// Dispatch a registered action by its full name.
    Action(String),
    /// Keystrokes, GPUI syntax.
    Keys(Vec<String>),
    /// Text typed into the focused element.
    Text(String),
    /// Wait until the condition holds.
    Wait(Condition, Duration),
    /// A screenshot point.
    Checkpoint(String),
    /// An assertion, recorded pass or fail.
    Expect(Condition),
}

impl StepKind {
    /// The kind's name in `results.json`.
    #[must_use]
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::Action(_) => "action",
            Self::Keys(_) => "keys",
            Self::Text(_) => "text",
            Self::Wait(..) => "wait",
            Self::Checkpoint(_) => "checkpoint",
            Self::Expect(_) => "expect",
        }
    }
}

/// One step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// The step's name in `results.json` (given, or derived from the step).
    pub name: String,
    /// What it does.
    pub kind: StepKind,
}

/// A parsed, checked scenario.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scenario {
    /// Short name (`[a-z0-9-]`).
    pub name: String,
    /// What it covers.
    pub description: String,
    /// `dev/engines.json`, resolved (inside the repository).
    pub engines: Option<PathBuf>,
    /// Whole-run timeout.
    pub timeout: Duration,
    /// Hold at each checkpoint.
    pub checkpoint_hold: Duration,
    /// Connections saved before the window opens.
    pub connections: Vec<ScenarioConnection>,
    /// DBeaver fixtures (directories of `crates/ninesql-dbeaver/tests/fixtures/`), resolved.
    pub dbeaver: Vec<PathBuf>,
    /// Files written in the run's HOME.
    pub files: Vec<ScenarioFile>,
    /// The steps.
    pub steps: Vec<Step>,
}

/// Why a scenario file was refused. Never echoes a text value of the file.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ScenarioError(pub String);

fn err(msg: impl Into<String>) -> ScenarioError {
    ScenarioError(msg.into())
}

/// `[a-z0-9-]{1,64}`: scenario and checkpoint names (they become file names of screenshots).
#[must_use]
pub fn is_plain_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// The repository root of a scenario file: its nearest ancestor holding `Cargo.toml` and
/// `dev/engines.json`.
#[must_use]
pub fn repository_root(scenario_file: &Path) -> Option<PathBuf> {
    scenario_file
        .ancestors()
        .skip(1)
        .find(|a| a.join("Cargo.toml").is_file() && a.join("dev/engines.json").is_file())
        .map(Path::to_path_buf)
}

/// `relative` resolved from `base`, canonical and inside `root`.
fn resolve_inside(
    base: &Path,
    relative: &str,
    root: &Path,
    what: &str,
) -> Result<PathBuf, ScenarioError> {
    let candidate = base.join(relative);
    let resolved =
        std::fs::canonicalize(&candidate).map_err(|_| err(format!("{what} does not exist")))?;
    let root = std::fs::canonicalize(root).map_err(|_| err("the repository cannot be read"))?;
    if !resolved.starts_with(&root) {
        return Err(err(format!("{what} must lie inside the repository")));
    }
    Ok(resolved)
}

impl Scenario {
    /// Reads and checks `path` (only that file and what it names inside the repository).
    ///
    /// # Errors
    /// [`ScenarioError`]: unreadable, too large, invalid TOML, an unknown action or probe key,
    /// a matcher that does not fit its key, a path outside the repository.
    pub fn load(path: &Path) -> Result<Self, ScenarioError> {
        let meta = std::fs::metadata(path).map_err(|_| err("the scenario file cannot be read"))?;
        if !meta.is_file() || meta.len() > MAX_SCENARIO_BYTES {
            return Err(err(
                "the scenario file is not a regular file of at most 1 MiB",
            ));
        }
        let text =
            std::fs::read_to_string(path).map_err(|_| err("the scenario file is not UTF-8"))?;
        let path =
            std::fs::canonicalize(path).map_err(|_| err("the scenario file cannot be read"))?;
        let root = repository_root(&path)
            .ok_or_else(|| err("the scenario file is not inside the NineSQL repository"))?;
        let base = path.parent().unwrap_or(&root).to_path_buf();
        Self::parse(&text, &base, &root)
    }

    /// Parses `text`; relative paths are resolved from `base` and must lie inside `root`.
    ///
    /// # Errors
    /// As [`Self::load`].
    pub fn parse(text: &str, base: &Path, root: &Path) -> Result<Self, ScenarioError> {
        let raw: RawScenario = toml::from_str(text).map_err(|e| {
            // TOML errors quote the offending line; keep only the position.
            let at = e
                .span()
                .map(|s| format!(" (byte {})", s.start))
                .unwrap_or_default();
            err(format!(
                "the scenario file is not valid{at}: {}",
                e.message()
            ))
        })?;
        if raw.schema_version != SCHEMA_VERSION {
            return Err(err(format!("schema_version must be {SCHEMA_VERSION}")));
        }
        if !is_plain_name(&raw.name) {
            return Err(err("name must be 1 to 64 characters among a-z, 0-9 and -"));
        }
        let timeout = match raw.timeout_secs {
            None => DEFAULT_RUN_TIMEOUT,
            Some(s) if s > 0 && Duration::from_secs(s) <= MAX_RUN_TIMEOUT => Duration::from_secs(s),
            Some(_) => return Err(err("timeout_secs must be 1 to 3600")),
        };
        let checkpoint_hold = match raw.checkpoint_hold_ms {
            None => DEFAULT_CHECKPOINT_HOLD,
            Some(ms) if ms <= 10_000 => Duration::from_millis(ms),
            Some(_) => return Err(err("checkpoint_hold_ms must be at most 10000")),
        };
        let engines = raw
            .engines
            .as_deref()
            .map(|e| resolve_inside(base, e, root, "engines"))
            .transpose()?;

        let mut connections = Vec::new();
        for c in raw.connection {
            if c.name.trim().is_empty()
                || c.name.len() > 128
                || c.name.chars().any(char::is_control)
            {
                return Err(err(
                    "a connection name must be 1 to 128 printable characters",
                ));
            }
            if connections
                .iter()
                .any(|x: &ScenarioConnection| x.name == c.name)
            {
                return Err(err("two connections have the same name"));
            }
            let kind = match c.ty.as_deref().unwrap_or("dev") {
                "dev" => ConnectionKind::Dev,
                "test" => ConnectionKind::Test,
                "prod" => ConnectionKind::Production,
                _ => {
                    return Err(err(
                        "a connection type must be \"dev\", \"test\" or \"prod\"",
                    ));
                }
            };
            if c.engine == SQLITE {
                match c.file.as_deref() {
                    Some(f) if is_file_name(f) => {}
                    _ => return Err(err("a sqlite connection needs a plain `file` name")),
                }
            } else if DEV_ENGINES.contains(&c.engine.as_str()) {
                if c.file.is_some() {
                    return Err(err("`file` is for sqlite connections only"));
                }
                if engines.is_none() {
                    return Err(err("a dev engine connection needs `engines`"));
                }
            } else {
                return Err(err(format!(
                    "engine must be one of {} or {SQLITE}",
                    DEV_ENGINES.join(", ")
                )));
            }
            connections.push(ScenarioConnection {
                name: c.name,
                engine: c.engine,
                kind,
                read_only: c.read_only,
                file: c.file,
            });
        }

        let fixtures_root = root.join("crates/ninesql-dbeaver/tests/fixtures");
        let mut dbeaver = Vec::new();
        for f in raw.dbeaver {
            if !is_file_name(&f.fixture) {
                return Err(err("a DBeaver fixture is named by its directory only"));
            }
            let dir = resolve_inside(
                &fixtures_root,
                &f.fixture,
                &fixtures_root,
                "the DBeaver fixture",
            )?;
            if !dir.join("workspace6").is_dir() {
                return Err(err("a DBeaver fixture must hold a workspace6 folder"));
            }
            dbeaver.push(dir);
        }
        if dbeaver.len() > 1 {
            return Err(err("at most one DBeaver fixture per scenario"));
        }

        let mut files = Vec::new();
        for f in raw.file {
            if !is_file_name(&f.name)
                || connections
                    .iter()
                    .any(|c| c.file.as_deref() == Some(&f.name))
            {
                return Err(err(
                    "a file name must be plain and distinct from the sqlite files",
                ));
            }
            if f.content.len() > MAX_FILE_BYTES {
                return Err(err("a file holds at most 1 MiB"));
            }
            files.push(ScenarioFile {
                name: f.name,
                content: f.content,
            });
        }

        if raw.step.is_empty() || raw.step.len() > MAX_STEPS {
            return Err(err(format!("a scenario has 1 to {MAX_STEPS} steps")));
        }
        let mut steps = Vec::new();
        let mut checkpoints = std::collections::BTreeSet::new();
        for (i, s) in raw.step.into_iter().enumerate() {
            let at = i + 1;
            let step = parse_step(s, at).map_err(|e| err(format!("step {at}: {}", e.0)))?;
            if let StepKind::Checkpoint(name) = &step.kind
                && !checkpoints.insert(name.clone())
            {
                return Err(err(format!("step {at}: checkpoint names must be distinct")));
            }
            steps.push(step);
        }
        Ok(Self {
            name: raw.name,
            description: raw.description,
            engines,
            timeout,
            checkpoint_hold,
            connections,
            dbeaver,
            files,
            steps,
        })
    }

    /// The dev engines (not sqlite) the scenario connects to.
    #[must_use]
    pub fn dev_engines(&self) -> Vec<&str> {
        let mut out: Vec<&str> = self
            .connections
            .iter()
            .map(|c| c.engine.as_str())
            .filter(|e| *e != SQLITE)
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }

    /// The checkpoint names, in order.
    #[must_use]
    pub fn checkpoints(&self) -> Vec<&str> {
        self.steps
            .iter()
            .filter_map(|s| match &s.kind {
                StepKind::Checkpoint(n) => Some(n.as_str()),
                _ => None,
            })
            .collect()
    }
}

/// A plain file name: no separator, not `.` or `..`, printable, at most 128 bytes.
fn is_file_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s != "."
        && s != ".."
        && !s.contains('/')
        && !s.contains('\\')
        && !s.chars().any(char::is_control)
}

fn parse_step(s: RawStep, at: usize) -> Result<Step, ScenarioError> {
    let kinds = [
        s.action.is_some(),
        s.keys.is_some(),
        s.text.is_some(),
        s.wait.is_some(),
        s.checkpoint.is_some(),
        s.expect.is_some(),
    ];
    if kinds.iter().filter(|k| **k).count() != 1 {
        return Err(err(
            "exactly one of action, keys, text, wait, checkpoint, expect",
        ));
    }
    let has_matcher =
        s.equals.is_some() || s.at_least.is_some() || s.at_most.is_some() || s.contains.is_some();
    let condition_only = || -> Result<(), ScenarioError> {
        if has_matcher || s.arg.is_some() || s.timeout_secs.is_some() {
            Err(err(
                "arg, matchers and timeout_secs are for wait and expect steps",
            ))
        } else {
            Ok(())
        }
    };
    let kind = if let Some(name) = s.action {
        condition_only()?;
        if !crate::actions::registry()
            .iter()
            .any(|spec| spec.name() == name)
        {
            return Err(err(format!(
                "`{name}` is not a registered action (full name)"
            )));
        }
        StepKind::Action(name)
    } else if let Some(keys) = s.keys {
        condition_only()?;
        let mut strokes = Vec::new();
        for stroke in keys.split_whitespace() {
            crate::keymap::validate_keys(stroke).map_err(err)?;
            strokes.push(stroke.to_owned());
        }
        if strokes.is_empty() || strokes.len() > 64 {
            return Err(err("keys holds 1 to 64 keystrokes"));
        }
        StepKind::Keys(strokes)
    } else if let Some(text) = s.text {
        condition_only()?;
        if text.is_empty() || text.len() > 64 * 1024 {
            return Err(err("text holds 1 to 65536 bytes"));
        }
        if text
            .chars()
            .any(|c| c.is_control() && c != '\n' && c != '\t')
        {
            return Err(err("text holds no control character but newlines and tabs"));
        }
        StepKind::Text(text)
    } else if let Some(name) = s.checkpoint {
        condition_only()?;
        if !is_plain_name(&name) {
            return Err(err(
                "a checkpoint name must be 1 to 64 characters among a-z, 0-9 and -",
            ));
        }
        StepKind::Checkpoint(name)
    } else {
        let (key, is_wait) = match (s.wait, s.expect) {
            (Some(k), None) => (k, true),
            (None, Some(k)) => (k, false),
            _ => return Err(err("exactly one of wait, expect")),
        };
        let probe =
            ProbeKey::parse(&key).ok_or_else(|| err(format!("`{key}` is not a probe key")))?;
        if probe.takes_arg() != s.arg.is_some() {
            return Err(err(if probe.takes_arg() {
                format!("`{key}` needs an arg")
            } else {
                format!("`{key}` takes no arg")
            }));
        }
        let mut matchers = Vec::new();
        if let Some(v) = s.equals {
            matchers.push(match v {
                toml::Value::Boolean(b) => Matcher::Bool(b),
                toml::Value::Integer(n) => Matcher::Int(n),
                toml::Value::String(t) => Matcher::Text(t),
                _ => return Err(err("equals is a boolean, an integer or a string")),
            });
        }
        matchers.extend(s.at_least.map(Matcher::AtLeast));
        matchers.extend(s.at_most.map(Matcher::AtMost));
        matchers.extend(s.contains.map(Matcher::Contains));
        if matchers.is_empty() {
            return Err(err(
                "a wait or expect step needs equals, at_least, at_most or contains",
            ));
        }
        if let Some(m) = matchers.iter().find(|m| !m.fits(probe.kind())) {
            return Err(err(format!(
                "`{key}` is {}; `{}` does not apply",
                probe.kind().name(),
                m.describe(false)
            )));
        }
        let condition = Condition {
            key: probe,
            arg: s.arg,
            matchers,
        };
        if is_wait {
            let timeout = match s.timeout_secs {
                None => DEFAULT_STEP_TIMEOUT,
                Some(t) if t > 0 && Duration::from_secs(t) <= MAX_STEP_TIMEOUT => {
                    Duration::from_secs(t)
                }
                Some(_) => return Err(err("timeout_secs must be 1 to 120")),
            };
            StepKind::Wait(condition, timeout)
        } else {
            if s.timeout_secs.is_some() {
                return Err(err("timeout_secs is for wait steps"));
            }
            StepKind::Expect(condition)
        }
    };
    let name = match s.name {
        Some(n) if is_plain_name(&n) => n,
        Some(_) => {
            return Err(err(
                "a step name must be 1 to 64 characters among a-z, 0-9 and -",
            ));
        }
        None => default_name(&kind, at),
    };
    Ok(Step { name, kind })
}

/// `<n>-<kind>[-<detail>]`, with no typed text in it.
fn default_name(kind: &StepKind, at: usize) -> String {
    match kind {
        StepKind::Action(a) => {
            let short = a.rsplit("::").next().unwrap_or(a);
            format!("{at}-action-{short}")
        }
        StepKind::Wait(c, _) => format!("{at}-wait-{}", c.key.name()),
        StepKind::Expect(c) => format!("{at}-expect-{}", c.key.name()),
        StepKind::Checkpoint(n) => format!("{at}-checkpoint-{n}"),
        StepKind::Keys(_) => format!("{at}-keys"),
        StepKind::Text(_) => format!("{at}-text"),
    }
}
