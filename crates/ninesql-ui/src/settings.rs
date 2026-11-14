//! `$XDG_CONFIG_HOME/ninesql/settings.toml`: appearance and key binding overrides
//! (ADR-0008 §1, F-WS-02, F-WS-03).
//!
//! ```toml
//! schema_version = 1
//!
//! [appearance]
//! theme = "dark"                  # "light", "dark" or "system"
//! ui_font_family = "Inter"
//! ui_font_size = 14               # pixels, 8 to 32
//! mono_font_family = "JetBrains Mono"
//! mono_font_size = 13
//!
//! [keybindings]
//! "ninesql::NewEditorTab" = "ctrl-n"              # replaces the defaults
//! SplitDown = ["ctrl-k ctrl-down", "alt-shift-2"] # short names work too
//! ToggleNavigator = []                            # unbinds
//! ```
//!
//! The file is hand-edited, so loading is lenient: a missing file means defaults; unknown keys
//! and sections are ignored; every invalid value or binding is reported as a [`Diagnostic`]
//! (shown in the status bar) and replaced by its default, never fatal. A file without
//! `schema_version` is read as version 1; a newer version is not read at all and the file is
//! left untouched (ADR-0008 §2: no silent downgrade). NineSQL does not write this file yet.
//!
//! Diagnostics name keys and key bindings, never other string values from the file.

use std::path::{Path, PathBuf};

use gpui_kit::Global;
use ninesql_core::storage::Paths;
use ninesql_core::storage::document::SCHEMA_VERSION_KEY;
use toml::{Table, Value};

use crate::actions;
use crate::keymap::{BindingOverrides, validate_keys};

/// Schema version of settings.toml this build reads.
pub const SETTINGS_SCHEMA_VERSION: i64 = 1;
/// Settings files larger than this are not read (hand-edited files are a few KiB).
const MAX_SETTINGS_BYTES: u64 = 1024 * 1024;
/// Allowed font sizes, in pixels.
pub const FONT_SIZE_RANGE: std::ops::RangeInclusive<f32> = 8.0..=32.0;

/// One problem found in settings.toml.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic(String);

impl Diagnostic {
    /// A problem with this message.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }

    /// The message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Theme choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThemePreference {
    /// Light theme.
    #[default]
    Light,
    /// Dark theme.
    Dark,
    /// Follow the desktop appearance at startup.
    System,
}

/// `[appearance]`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Appearance {
    /// Theme.
    pub theme: ThemePreference,
    /// Interface font family; `None` keeps the system UI font.
    pub ui_font_family: Option<String>,
    /// Interface font size in pixels; `None` keeps the theme default.
    pub ui_font_size: Option<f32>,
    /// Editor font family; `None` keeps the theme default monospace font.
    pub mono_font_family: Option<String>,
    /// Editor font size in pixels; `None` keeps the theme default.
    pub mono_font_size: Option<f32>,
}

/// The parsed settings.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Settings {
    /// `[appearance]`.
    pub appearance: Appearance,
    /// `[keybindings]`, validated.
    pub keybindings: BindingOverrides,
}

/// Settings with the problems found while reading them, kept as a GPUI global.
#[derive(Debug, Clone, Default)]
pub struct SettingsState {
    /// Where the settings were read from, if a location could be resolved.
    pub path: Option<PathBuf>,
    /// The settings in effect.
    pub settings: Settings,
    /// Problems, in file order.
    pub diagnostics: Vec<Diagnostic>,
}

impl Global for SettingsState {}

/// Parses settings text. Never fails: problems are returned next to the settings in effect.
#[must_use]
pub fn parse(text: &str) -> (Settings, Vec<Diagnostic>) {
    let mut diags = Vec::new();
    let mut settings = Settings::default();
    let table: Table = match text.parse() {
        Ok(table) => table,
        Err(err) => {
            let line = err
                .span()
                .map(|span| text[..span.start.min(text.len())].lines().count().max(1));
            diags.push(Diagnostic::new(match line {
                Some(line) => {
                    format!("settings.toml is not valid TOML (line {line}): defaults are used")
                }
                None => "settings.toml is not valid TOML: defaults are used".to_owned(),
            }));
            return (settings, diags);
        }
    };

    match table.get(SCHEMA_VERSION_KEY) {
        None => {}
        Some(Value::Integer(v)) if *v == SETTINGS_SCHEMA_VERSION => {}
        Some(Value::Integer(v)) if *v > SETTINGS_SCHEMA_VERSION => {
            diags.push(Diagnostic::new(format!(
                "settings.toml has schema_version {v}, newer than this NineSQL reads \
                 ({SETTINGS_SCHEMA_VERSION}): the file is ignored and left untouched"
            )));
            return (settings, diags);
        }
        Some(_) => diags.push(Diagnostic::new(format!(
            "{SCHEMA_VERSION_KEY} must be {SETTINGS_SCHEMA_VERSION}: read as version {SETTINGS_SCHEMA_VERSION}"
        ))),
    }

    match table.get("appearance") {
        None => {}
        Some(Value::Table(t)) => settings.appearance = parse_appearance(t, &mut diags),
        Some(_) => diags.push(Diagnostic::new("[appearance] must be a table")),
    }
    match table.get("keybindings") {
        None => {}
        Some(Value::Table(t)) => settings.keybindings = parse_keybindings(t, &mut diags),
        Some(_) => diags.push(Diagnostic::new("[keybindings] must be a table")),
    }
    (settings, diags)
}

fn parse_appearance(t: &Table, diags: &mut Vec<Diagnostic>) -> Appearance {
    let mut out = Appearance::default();
    if let Some(v) = t.get("theme") {
        match v.as_str() {
            Some("light") => out.theme = ThemePreference::Light,
            Some("dark") => out.theme = ThemePreference::Dark,
            Some("system") => out.theme = ThemePreference::System,
            _ => diags.push(Diagnostic::new(
                "[appearance] theme must be \"light\", \"dark\" or \"system\": light is used",
            )),
        }
    }
    out.ui_font_family = font_family(t, "ui_font_family", diags);
    out.mono_font_family = font_family(t, "mono_font_family", diags);
    out.ui_font_size = font_size(t, "ui_font_size", diags);
    out.mono_font_size = font_size(t, "mono_font_size", diags);
    out
}

fn font_family(t: &Table, key: &str, diags: &mut Vec<Diagnostic>) -> Option<String> {
    let v = t.get(key)?;
    match v.as_str().map(str::trim) {
        Some(name) if !name.is_empty() && name.len() <= 128 => Some(name.to_owned()),
        _ => {
            diags.push(Diagnostic::new(format!(
                "[appearance] {key} must be a non-empty font name: the default is used"
            )));
            None
        }
    }
}

fn font_size(t: &Table, key: &str, diags: &mut Vec<Diagnostic>) -> Option<f32> {
    let v = t.get(key)?;
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    let size = match v {
        Value::Integer(i) => Some(*i as f32),
        Value::Float(f) => Some(*f as f32),
        _ => None,
    };
    match size {
        Some(size) if FONT_SIZE_RANGE.contains(&size) => Some(size),
        _ => {
            diags.push(Diagnostic::new(format!(
                "[appearance] {key} must be a number from {} to {}: the default is used",
                FONT_SIZE_RANGE.start(),
                FONT_SIZE_RANGE.end()
            )));
            None
        }
    }
}

fn parse_keybindings(t: &Table, diags: &mut Vec<Diagnostic>) -> BindingOverrides {
    let mut out = BindingOverrides::default();
    for (name, value) in t {
        let Some(spec) = actions::find(name) else {
            diags.push(Diagnostic::new(format!(
                "[keybindings] unknown action `{name}`: ignored"
            )));
            continue;
        };
        let raw: Option<Vec<&str>> = match value {
            Value::String(s) => Some(vec![s.as_str()]),
            Value::Array(items) => items.iter().map(Value::as_str).collect(),
            _ => None,
        };
        let Some(raw) = raw else {
            diags.push(Diagnostic::new(format!(
                "[keybindings] {}: expected a key binding or a list of key bindings; \
                 the defaults are kept",
                spec.name()
            )));
            continue;
        };
        let mut keys = Vec::with_capacity(raw.len());
        let mut valid = true;
        for k in raw {
            match validate_keys(k) {
                Ok(()) => keys.push(k.split_whitespace().collect::<Vec<_>>().join(" ")),
                Err(why) => {
                    diags.push(Diagnostic::new(format!(
                        "[keybindings] {}: {why}; the defaults are kept",
                        spec.name()
                    )));
                    valid = false;
                }
            }
        }
        if valid {
            out.set(spec.name(), keys);
        }
    }
    out
}

/// Reads settings from `path`. A missing file gives defaults without a problem.
#[must_use]
pub fn load(path: &Path) -> SettingsState {
    let mut state = SettingsState {
        path: Some(path.to_path_buf()),
        ..SettingsState::default()
    };
    match ninesql_core::storage::fs::read_limited(path, MAX_SETTINGS_BYTES) {
        Ok(None) => {}
        Ok(Some(bytes)) => match String::from_utf8(bytes) {
            Ok(text) => {
                let (settings, diagnostics) = parse(&text);
                state.settings = settings;
                state.diagnostics = diagnostics;
            }
            Err(_) => state.diagnostics.push(Diagnostic::new(
                "settings.toml is not valid UTF-8: defaults are used",
            )),
        },
        Err(err) => state.diagnostics.push(Diagnostic::new(format!(
            "settings.toml cannot be read ({err}): defaults are used"
        ))),
    }
    for d in &state.diagnostics {
        tracing::warn!(problem = %d, "settings.toml");
    }
    state
}

/// Reads settings from the XDG location (ADR-0008).
#[must_use]
pub fn load_from_env() -> SettingsState {
    match Paths::from_env() {
        Ok(paths) => load(&paths.settings_file()),
        Err(err) => SettingsState {
            path: None,
            settings: Settings::default(),
            diagnostics: vec![Diagnostic::new(format!(
                "no settings location ({err}): defaults are used"
            ))],
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{ThemePreference, load, parse};

    #[test]
    fn empty_text_gives_defaults() {
        let (settings, diags) = parse("");
        assert_eq!(settings, super::Settings::default());
        assert!(diags.is_empty());
    }

    #[test]
    fn full_file() {
        let (settings, diags) = parse(
            r#"
schema_version = 1
[appearance]
theme = "dark"
ui_font_family = "Inter"
ui_font_size = 15
mono_font_family = "JetBrains Mono"
mono_font_size = 13.5

[keybindings]
"ninesql::NewEditorTab" = "ctrl-n"
SplitDown = ["ctrl-k   ctrl-down", "alt-shift-2"]
ToggleNavigator = []
"#,
        );
        assert!(diags.is_empty(), "{diags:?}");
        let a = &settings.appearance;
        assert_eq!(a.theme, ThemePreference::Dark);
        assert_eq!(a.ui_font_family.as_deref(), Some("Inter"));
        assert_eq!(a.ui_font_size, Some(15.0));
        assert_eq!(a.mono_font_size, Some(13.5));
        let kb = &settings.keybindings;
        assert_eq!(
            kb.get("ninesql::NewEditorTab"),
            Some(&["ctrl-n".to_owned()][..])
        );
        assert_eq!(
            kb.get("ninesql::SplitDown"),
            Some(&["ctrl-k ctrl-down".to_owned(), "alt-shift-2".to_owned()][..])
        );
        assert_eq!(kb.get("ninesql::ToggleNavigator"), Some(&[][..]));
    }

    #[test]
    fn unknown_keys_are_tolerated() {
        let (settings, diags) = parse(
            r#"
future_option = true
[appearance]
theme = "system"
sparkles = 3
[editor]
tab_size = 4
"#,
        );
        assert!(diags.is_empty(), "{diags:?}");
        assert_eq!(settings.appearance.theme, ThemePreference::System);
    }

    #[test]
    fn invalid_bindings_are_reported_not_fatal() {
        let (settings, diags) = parse(
            r#"
[keybindings]
NewEditorTab = "ctrl-foo"
CloseTab = ["ctrl-w", 3]
NoSuchAction = "ctrl-x"
SplitRight = "ctrl-alt-s"
"#,
        );
        assert_eq!(diags.len(), 3, "{diags:?}");
        assert!(
            diags
                .iter()
                .any(|d| d.message().contains("unknown key `foo`"))
        );
        assert!(
            diags
                .iter()
                .any(|d| d.message().contains("unknown action `NoSuchAction`"))
        );
        assert!(
            diags
                .iter()
                .any(|d| d.message().contains("ninesql::CloseTab: expected"))
        );
        // Invalid entries keep their defaults; the valid one applies.
        assert!(settings.keybindings.get("ninesql::NewEditorTab").is_none());
        assert!(settings.keybindings.get("ninesql::CloseTab").is_none());
        assert_eq!(
            settings.keybindings.get("ninesql::SplitRight"),
            Some(&["ctrl-alt-s".to_owned()][..])
        );
    }

    #[test]
    fn invalid_appearance_values_fall_back() {
        let (settings, diags) = parse(
            r#"
[appearance]
theme = "solarized"
ui_font_size = 300
mono_font_size = "big"
ui_font_family = ""
"#,
        );
        assert_eq!(diags.len(), 4, "{diags:?}");
        assert_eq!(settings.appearance, super::Appearance::default());
        // Values from the file are not echoed back.
        assert!(!diags.iter().any(|d| d.message().contains("solarized")));
    }

    #[test]
    fn broken_toml_and_newer_versions() {
        let (settings, diags) = parse("[appearance\ntheme = 'dark'");
        assert_eq!(settings, super::Settings::default());
        assert_eq!(diags.len(), 1);
        assert!(diags[0].message().contains("line 1"), "{diags:?}");

        let (settings, diags) = parse("schema_version = 2\n[appearance]\ntheme = 'dark'");
        assert_eq!(settings.appearance.theme, ThemePreference::Light);
        assert!(diags[0].message().contains("newer"));

        let (_, diags) = parse("schema_version = 'one'");
        assert_eq!(diags.len(), 1);
    }

    #[test]
    fn load_missing_and_present_files() {
        let dir = std::env::temp_dir().join(format!("ninesql-ui-settings-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("settings.toml");
        let _ = std::fs::remove_file(&path);
        let state = load(&path);
        assert!(state.diagnostics.is_empty());
        assert_eq!(state.settings, super::Settings::default());

        assert!(std::fs::write(&path, "[appearance]\ntheme = \"dark\"\n").is_ok());
        let state = load(&path);
        assert!(state.diagnostics.is_empty(), "{:?}", state.diagnostics);
        assert_eq!(state.settings.appearance.theme, ThemePreference::Dark);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
