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
//! zoom = 0                        # pixels added to both sizes by the zoom actions, -8 to 16
//!
//! [keybindings]
//! "ninesql::NewEditorTab" = "ctrl-n"              # replaces the defaults
//! SplitDown = ["ctrl-k ctrl-down", "alt-shift-2"] # short names work too
//! ToggleNavigator = []                            # unbinds
//!
//! [import]
//! first_run_prompt = false        # do not offer the DBeaver import at startup (P2-B)
//! ```
//!
//! The file is hand-edited, so loading is lenient: a missing file means defaults; unknown keys
//! and sections are ignored; every invalid value or binding is reported as a [`Diagnostic`]
//! (shown in the status bar) and replaced by its default, never fatal. A file without
//! `schema_version` is read as version 1; a newer version is not read at all and the file is
//! left untouched (ADR-0008 §2: no silent downgrade).
//!
//! Changes made from the UI (theme, fonts, zoom, key bindings) are written back by
//! [`crate::persist`], which edits only the keys that changed and keeps comments, formatting and
//! unknown keys. NineSQL never writes a file it could not read: a newer `schema_version`, invalid
//! TOML or UTF-8, an unreadable file or a refused symbolic link block saving (see
//! [`SettingsState::save_blocked`]) and the problem is listed with the others.
//!
//! # Symbolic links
//! Dotfile managers often make `settings.toml` a symbolic link. NineSQL follows it only if the
//! final target (every link resolved) is a regular file owned by the current effective user
//! ([`ninesql_core::storage::fs::current_uid`]); it then reads and writes that target, so the link
//! itself stays in place (the atomic write renames a temp file next to the target, and the target
//! ends up with mode 0600). A link to a file owned by someone else (for example a root-owned file
//! in `/nix/store`), to something that is not a regular file, or a dangling link is refused with
//! a diagnostic: defaults are used and nothing is written. See [`resolve_settings_file`].
//!
//! Diagnostics name keys and key bindings, never other string values from the file.

use std::io;
use std::os::unix::fs::MetadataExt as _;
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
/// Allowed zoom offsets, in pixels.
pub const ZOOM_RANGE: std::ops::RangeInclusive<f32> = -8.0..=16.0;

/// One problem found in settings.toml.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    message: String,
    /// What a log event carries instead of the message, when the message holds a storage
    /// error's text (a full path names the user's home, ADR-0029 §3.2).
    log: Option<String>,
}

impl Diagnostic {
    /// A problem with this message, safe to log as it is.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            log: None,
        }
    }

    /// A problem whose message is for the UI only; log events carry `log` instead.
    #[must_use]
    pub fn with_log_text(message: impl Into<String>, log: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            log: Some(log.into()),
        }
    }

    /// The message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// The text for log events: no path, no storage detail.
    #[must_use]
    pub fn log_text(&self) -> &str {
        self.log.as_deref().unwrap_or(&self.message)
    }
}

impl std::fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// A storage error's kind (and I/O kind), safe to log: never its text, which holds a path.
#[must_use]
pub(crate) fn storage_reason(e: &ninesql_core::storage::StorageError) -> String {
    match e.io_kind() {
        Some(io) => format!("{}: {io}", e.kind()),
        None => e.kind().to_owned(),
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
    /// Pixels added to both font sizes by the zoom actions.
    pub zoom: f32,
}

/// `[import]` (P2-B).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImportSettings {
    /// Offer the DBeaver import when the project has no connection and a DBeaver workspace is
    /// found (`first_run_prompt`, default `true`). Set to `false` by "Don't show again".
    pub first_run_prompt: bool,
}

impl Default for ImportSettings {
    fn default() -> Self {
        Self {
            first_run_prompt: true,
        }
    }
}

/// The parsed settings.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Settings {
    /// `[appearance]`.
    pub appearance: Appearance,
    /// `[keybindings]`, validated.
    pub keybindings: BindingOverrides,
    /// `[import]`.
    pub import: ImportSettings,
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
    /// Why changes made from the UI are not written to the file, if they are not. Set when the
    /// file could not be read as this build's settings: NineSQL never overwrites what it could
    /// not read.
    pub save_blocked: Option<Diagnostic>,
}

impl Global for SettingsState {}

/// Parses settings text. Never fails: problems are returned next to the settings in effect.
#[must_use]
pub fn parse(text: &str) -> (Settings, Vec<Diagnostic>) {
    let (settings, diags, _) = parse_full(text);
    (settings, diags)
}

/// [`parse`], and whether the text must not be overwritten (invalid TOML, newer schema).
fn parse_full(text: &str) -> (Settings, Vec<Diagnostic>, Option<Diagnostic>) {
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
            return (
                settings,
                diags,
                Some(not_saved("settings.toml is not valid TOML")),
            );
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
            return (
                settings,
                diags,
                Some(not_saved("settings.toml was written by a newer NineSQL")),
            );
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
    match table.get("import") {
        None => {}
        Some(Value::Table(t)) => settings.import = parse_import(t, &mut diags),
        Some(_) => diags.push(Diagnostic::new("[import] must be a table")),
    }
    (settings, diags, None)
}

/// The "changes are not saved" diagnostic for `why`.
fn not_saved(why: &str) -> Diagnostic {
    Diagnostic::new(format!(
        "{why}: changes made in NineSQL are not saved to settings.toml"
    ))
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
    if let Some(v) = t.get("zoom") {
        #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
        match v {
            Value::Integer(i) if ZOOM_RANGE.contains(&(*i as f32)) => out.zoom = *i as f32,
            Value::Float(f) if ZOOM_RANGE.contains(&(*f as f32)) => out.zoom = *f as f32,
            _ => diags.push(Diagnostic::new(format!(
                "[appearance] zoom must be a number from {} to {}: 0 is used",
                ZOOM_RANGE.start(),
                ZOOM_RANGE.end()
            ))),
        }
    }
    out
}

fn parse_import(t: &Table, diags: &mut Vec<Diagnostic>) -> ImportSettings {
    let mut out = ImportSettings::default();
    if let Some(v) = t.get("first_run_prompt") {
        match v.as_bool() {
            Some(b) => out.first_run_prompt = b,
            None => diags.push(Diagnostic::new(
                "[import] first_run_prompt must be true or false: true is used",
            )),
        }
    }
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

/// The file to read and write for the configured settings location `path`, under the
/// symbolic link rule of the module documentation:
///
/// - a missing file or a regular file: `path` itself;
/// - a symbolic link whose final target is a regular file owned by the current effective user:
///   that target (canonical path, no link left in it);
/// - anything else (a link to a file owned by someone else or to a non-regular file, a dangling
///   link, a directory or a device in place of the file): refused.
///
/// # Errors
/// A diagnostic for the status bar; the caller uses defaults and writes nothing.
pub fn resolve_settings_file(path: &Path) -> Result<PathBuf, Diagnostic> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(path.to_path_buf()),
        Err(e) => {
            return Err(Diagnostic::new(format!(
                "settings.toml cannot be read ({e}): defaults are used"
            )));
        }
    };
    if meta.file_type().is_file() {
        return Ok(path.to_path_buf());
    }
    if !meta.file_type().is_symlink() {
        return Err(Diagnostic::new(
            "settings.toml is not a regular file: defaults are used",
        ));
    }
    let target = match std::fs::canonicalize(path) {
        Ok(target) => target,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(Diagnostic::new(
                "settings.toml is a symbolic link to a missing file: defaults are used",
            ));
        }
        Err(e) => {
            return Err(Diagnostic::new(format!(
                "settings.toml is a symbolic link that cannot be followed ({e}): defaults are used"
            )));
        }
    };
    let meta = std::fs::symlink_metadata(&target).map_err(|e| {
        Diagnostic::new(format!(
            "settings.toml is a symbolic link that cannot be followed ({e}): defaults are used"
        ))
    })?;
    if !meta.file_type().is_file() {
        return Err(Diagnostic::new(
            "settings.toml is a symbolic link to something that is not a regular file: \
             defaults are used",
        ));
    }
    let me = ninesql_core::storage::fs::current_uid();
    if meta.uid() != me {
        return Err(Diagnostic::new(format!(
            "settings.toml is a symbolic link to a file owned by another user (uid {}): \
             defaults are used",
            meta.uid()
        )));
    }
    tracing::debug!(link = %path.display(), target = %target.display(), "following the settings.toml symbolic link");
    Ok(target)
}

/// Reads settings from `path` (the configured location; a symbolic link is followed under the
/// rule of [`resolve_settings_file`]). A missing file gives defaults without a problem.
#[must_use]
pub fn load(path: &Path) -> SettingsState {
    let mut state = SettingsState {
        path: Some(path.to_path_buf()),
        ..SettingsState::default()
    };
    match resolve_settings_file(path) {
        Err(problem) => {
            state.save_blocked = Some(not_saved("settings.toml was not read"));
            state.diagnostics.push(problem);
        }
        Ok(file) => match ninesql_core::storage::fs::read_limited(&file, MAX_SETTINGS_BYTES) {
            Ok(None) => {}
            Ok(Some(bytes)) => match String::from_utf8(bytes) {
                Ok(text) => {
                    let (settings, diagnostics, blocked) = parse_full(&text);
                    state.settings = settings;
                    state.diagnostics = diagnostics;
                    state.save_blocked = blocked;
                }
                Err(_) => {
                    state.diagnostics.push(Diagnostic::new(
                        "settings.toml is not valid UTF-8: defaults are used",
                    ));
                    state.save_blocked = Some(not_saved("settings.toml is not valid UTF-8"));
                }
            },
            Err(err) => {
                state.diagnostics.push(Diagnostic::with_log_text(
                    format!("settings.toml cannot be read ({err}): defaults are used"),
                    format!(
                        "settings.toml cannot be read ({}): defaults are used",
                        storage_reason(&err)
                    ),
                ));
                state.save_blocked = Some(not_saved("settings.toml was not read"));
            }
        },
    }
    for d in &state.diagnostics {
        tracing::warn!(problem = %d.log_text(), "settings.toml");
    }
    state
}

/// Reads settings from the location of `paths` (ADR-0008).
#[must_use]
pub fn load_from_paths(paths: &Paths) -> SettingsState {
    load(&paths.settings_file())
}

/// Reads settings from the XDG location (ADR-0008).
#[must_use]
pub fn load_from_env() -> SettingsState {
    match Paths::from_env() {
        Ok(paths) => load_from_paths(&paths),
        Err(err) => SettingsState {
            path: None,
            settings: Settings::default(),
            diagnostics: vec![Diagnostic::new(format!(
                "no settings location ({err}): defaults are used"
            ))],
            save_blocked: Some(not_saved("no settings location")),
        },
    }
}

/// The largest settings file NineSQL reads, in bytes.
#[must_use]
pub const fn max_settings_bytes() -> u64 {
    MAX_SETTINGS_BYTES
}

#[cfg(test)]
mod tests {
    use super::{ThemePreference, load, parse, resolve_settings_file};

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
    fn core_preference_sections_are_not_flagged() {
        // `[execution]`, `[history]` and `[confirmations]` belong to the core's preferences
        // (P3-C); the UI's diagnostics leave them alone.
        let (_, diags) = parse(
            r#"
[execution]
default_timeout_secs = 30
[history]
enabled = false
[confirmations]
close_with_open_transaction = true
[keybindings]
"ninesql_editor::ExecuteStatement" = "f5"
"ninesql_grid::FetchAllRows" = []
"#,
        );
        assert!(diags.is_empty(), "{diags:?}");
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

    fn temp_dir() -> tempfile::TempDir {
        match tempfile::tempdir() {
            Ok(dir) => dir,
            Err(e) => panic!("no temp dir: {e}"),
        }
    }

    #[test]
    fn owned_symlink_is_followed() {
        let dir = temp_dir();
        let target = dir.path().join("dotfiles-settings.toml");
        assert!(std::fs::write(&target, "[appearance]\ntheme = \"dark\"\n").is_ok());
        let link = dir.path().join("settings.toml");
        assert!(std::os::unix::fs::symlink(&target, &link).is_ok());
        let resolved = resolve_settings_file(&link);
        assert_eq!(resolved.ok(), std::fs::canonicalize(&target).ok());
        let state = load(&link);
        assert!(state.diagnostics.is_empty(), "{:?}", state.diagnostics);
        assert!(state.save_blocked.is_none());
        assert_eq!(state.settings.appearance.theme, ThemePreference::Dark);
        // A chain of links is followed to its final target.
        let link2 = dir.path().join("settings2.toml");
        assert!(std::os::unix::fs::symlink(&link, &link2).is_ok());
        assert_eq!(
            load(&link2).settings.appearance.theme,
            ThemePreference::Dark
        );
    }

    #[test]
    fn symlink_to_a_foreign_file_is_refused() {
        let dir = temp_dir();
        let target = if ninesql_core::storage::fs::current_uid() == 0 {
            // Running as root (containers): hand a file to `nobody`.
            let target = dir.path().join("foreign.toml");
            assert!(std::fs::write(&target, "[appearance]\ntheme = \"dark\"\n").is_ok());
            assert!(std::os::unix::fs::chown(&target, Some(65534), Some(65534)).is_ok());
            target
        } else {
            // Any root-owned regular file.
            std::path::PathBuf::from("/etc/passwd")
        };
        let link = dir.path().join("settings.toml");
        assert!(std::os::unix::fs::symlink(&target, &link).is_ok());
        let state = load(&link);
        assert_eq!(state.settings, super::Settings::default());
        assert!(state.save_blocked.is_some());
        assert!(
            state.diagnostics[0]
                .message()
                .contains("owned by another user"),
            "{:?}",
            state.diagnostics
        );
        // Saving refuses too, and the link is untouched.
        let saved = crate::persist::save_blocking(
            &link,
            &[crate::persist::SettingsChange::Theme(
                ThemePreference::Light,
            )],
        );
        assert!(saved.is_err());
        assert!(std::fs::symlink_metadata(&link).is_ok_and(|m| m.file_type().is_symlink()));
    }

    #[test]
    fn dangling_symlink_and_non_files_are_refused() {
        let dir = temp_dir();
        let link = dir.path().join("settings.toml");
        assert!(std::os::unix::fs::symlink(dir.path().join("missing.toml"), &link).is_ok());
        let state = load(&link);
        assert!(state.save_blocked.is_some());
        assert!(
            state.diagnostics[0].message().contains("missing file"),
            "{:?}",
            state.diagnostics
        );
        let saved =
            crate::persist::save_blocking(&link, &[crate::persist::SettingsChange::Zoom(1.0)]);
        assert!(saved.is_err());
        assert!(
            !dir.path().join("missing.toml").exists(),
            "nothing is created through the link"
        );

        // A link to a directory, and a directory in place of the file.
        let link_dir = dir.path().join("to-dir.toml");
        assert!(std::os::unix::fs::symlink(dir.path(), &link_dir).is_ok());
        assert!(resolve_settings_file(&link_dir).is_err());
        let as_dir = dir.path().join("dir.toml");
        assert!(std::fs::create_dir(&as_dir).is_ok());
        assert!(resolve_settings_file(&as_dir).is_err());
    }

    #[test]
    fn zoom_is_read_and_checked() {
        let (settings, diags) = parse("[appearance]\nzoom = -2\n");
        assert!(diags.is_empty(), "{diags:?}");
        assert_eq!(settings.appearance.zoom, -2.0);
        let (settings, diags) = parse("[appearance]\nzoom = 40\n");
        assert_eq!(diags.len(), 1);
        assert_eq!(settings.appearance.zoom, 0.0);
    }
}
