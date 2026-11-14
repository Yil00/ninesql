//! Writes settings changed from the UI back to `settings.toml` (F-WS-02, F-WS-03).
//!
//! - **Only what changed.** Each change from the UI is a [`SettingsChange`]; saving reads the
//!   current file, edits those keys with `toml_edit` and leaves everything else as it is:
//!   comments, formatting, key order, unknown keys and sections, entries the UI did not touch
//!   (including invalid ones, which keep being reported). Edits made by hand while NineSQL runs
//!   are therefore kept, except for the keys the UI changes afterwards.
//! - **Never on the UI thread.** [`record`] only queues the change; after [`DEBOUNCE`] without a
//!   new change, the queue is written by a task on GPUI's background executor (I6). One write
//!   runs at a time, in order, and the last pending changes are written when NineSQL quits.
//! - **Atomic and private.** The file is written with [`write_atomic`] (temp file `0600`,
//!   `fsync`, rename). A missing configuration directory is created `0700`; an existing real
//!   directory gets its group / other bits removed and must be owned by the current user. A
//!   symbolically linked directory (dotfile managers) is left as it is.
//! - **`schema_version`.** Kept when present, added (`1`) when missing. A file with a newer
//!   version, invalid TOML or UTF-8 is never overwritten: [`SettingsState::save_blocked`] is set
//!   at startup, and a file that became unreadable since then makes the save fail with a
//!   diagnostic instead.
//! - **Symbolic links.** The file is resolved with [`resolve_settings_file`] again at every save
//!   (see the rule in [`crate::settings`]); a followed link is written through its target.
//!
//! Settings hold no secret (I1); the file is still written `0600` like every NineSQL file.

use std::io;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use gpui_kit::{App, Global, Task};
use ninesql_core::storage::document::SCHEMA_VERSION_KEY;
use ninesql_core::storage::fs::{ensure_private_dir, read_limited, write_atomic};
use toml_edit::{DocumentMut, Item, TableLike, Value};

use crate::actions;
use crate::settings::{
    Diagnostic, SETTINGS_SCHEMA_VERSION, Settings, SettingsState, ThemePreference,
    max_settings_bytes, resolve_settings_file,
};

/// Quiet time after the last change before the file is written.
pub const DEBOUNCE: Duration = Duration::from_millis(500);

/// Serializes read-modify-write cycles on the file (the debounced writer and the quit flush).
static SAVE_LOCK: Mutex<()> = Mutex::new(());

/// One setting changed from the UI.
#[derive(Debug, Clone, PartialEq)]
pub enum SettingsChange {
    /// `[appearance] theme`.
    Theme(ThemePreference),
    /// `[appearance] ui_font_family`; `None` removes the key.
    UiFontFamily(Option<String>),
    /// `[appearance] ui_font_size`; `None` removes the key.
    UiFontSize(Option<f32>),
    /// `[appearance] mono_font_family`; `None` removes the key.
    MonoFontFamily(Option<String>),
    /// `[appearance] mono_font_size`; `None` removes the key.
    MonoFontSize(Option<f32>),
    /// `[appearance] zoom`.
    Zoom(f32),
    /// `[keybindings]` entry of an action (full name): `Some` overrides the defaults (an empty
    /// list unbinds the action), `None` removes the entry so the defaults apply again.
    Keybinding(&'static str, Option<Vec<String>>),
}

impl SettingsChange {
    /// Two changes with the same key replace each other in the queue.
    fn key(&self) -> (&'static str, &'static str) {
        match self {
            Self::Theme(_) => ("appearance", "theme"),
            Self::UiFontFamily(_) => ("appearance", "ui_font_family"),
            Self::UiFontSize(_) => ("appearance", "ui_font_size"),
            Self::MonoFontFamily(_) => ("appearance", "mono_font_family"),
            Self::MonoFontSize(_) => ("appearance", "mono_font_size"),
            Self::Zoom(_) => ("appearance", "zoom"),
            Self::Keybinding(action, _) => ("keybindings", action),
        }
    }

    /// Applies the change to the settings in memory.
    pub fn apply_to(&self, settings: &mut Settings) {
        let a = &mut settings.appearance;
        match self {
            Self::Theme(t) => a.theme = *t,
            Self::UiFontFamily(f) => a.ui_font_family.clone_from(f),
            Self::UiFontSize(s) => a.ui_font_size = *s,
            Self::MonoFontFamily(f) => a.mono_font_family.clone_from(f),
            Self::MonoFontSize(s) => a.mono_font_size = *s,
            Self::Zoom(z) => a.zoom = *z,
            Self::Keybinding(action, Some(keys)) => settings.keybindings.set(action, keys.clone()),
            Self::Keybinding(action, None) => settings.keybindings.remove(action),
        }
    }
}

/// Changes waiting to be written, as a GPUI global.
#[derive(Default)]
struct Writer {
    pending: Vec<SettingsChange>,
    /// A write loop is running; it picks up whatever is pending when its write ends.
    writing: bool,
    /// The debounce timer; replacing it cancels the previous one.
    debounce: Option<Task<()>>,
    /// The diagnostic of the last failed save, removed again after a successful one.
    last_error: Option<Diagnostic>,
}

impl Global for Writer {}

/// Applies `change` to the settings in effect and queues it for writing. Returns at once: the
/// file is written on the background executor after [`DEBOUNCE`]. Nothing is queued when saving
/// is blocked ([`SettingsState::save_blocked`]) or there is no settings location.
pub fn record(change: SettingsChange, cx: &mut App) {
    let state = cx.default_global::<SettingsState>();
    change.apply_to(&mut state.settings);
    if state.path.is_none() || state.save_blocked.is_some() {
        tracing::debug!("settings change kept in memory only: saving is blocked");
        return;
    }
    let writer = cx.default_global::<Writer>();
    let key = change.key();
    writer.pending.retain(|c| c.key() != key);
    writer.pending.push(change);
    let timer = cx.background_executor().timer(DEBOUNCE);
    let task = cx.spawn(async move |cx| {
        timer.await;
        cx.update(flush);
    });
    cx.default_global::<Writer>().debounce = Some(task);
}

/// Whether changes are waiting to be written or being written.
#[must_use]
pub fn has_pending(cx: &App) -> bool {
    cx.try_global::<Writer>()
        .is_some_and(|w| w.writing || !w.pending.is_empty())
}

/// Starts writing the pending changes now (on the background executor), without waiting for
/// the debounce timer.
pub fn flush(cx: &mut App) {
    let Some(path) = cx
        .try_global::<SettingsState>()
        .and_then(|s| s.path.clone())
    else {
        return;
    };
    let writer = cx.default_global::<Writer>();
    if writer.writing || writer.pending.is_empty() {
        return;
    }
    writer.writing = true;
    cx.spawn(async move |cx| {
        loop {
            let changes = cx.update(|cx| {
                let writer = cx.default_global::<Writer>();
                let changes = std::mem::take(&mut writer.pending);
                if changes.is_empty() {
                    writer.writing = false;
                }
                changes
            });
            if changes.is_empty() {
                break;
            }
            let path = path.clone();
            let result = cx
                .background_executor()
                .spawn(async move { save_blocking(&path, &changes) })
                .await;
            cx.update(|cx| report(result, cx));
        }
    })
    .detach();
}

/// Writes whatever is still queued when NineSQL quits. GPUI waits for the returned future up to
/// its shutdown timeout; the write itself runs on the background executor.
pub fn flush_on_quit(cx: &mut App) -> impl std::future::Future<Output = ()> + use<> {
    let path = cx
        .try_global::<SettingsState>()
        .and_then(|s| s.path.clone());
    let changes = std::mem::take(&mut cx.default_global::<Writer>().pending);
    let task = match path {
        Some(path) if !changes.is_empty() => Some(
            cx.background_executor()
                .spawn(async move { save_blocking(&path, &changes) }),
        ),
        _ => None,
    };
    async move {
        if let Some(task) = task
            && let Err(problem) = task.await
        {
            tracing::warn!(problem = %problem, "settings not saved at quit");
        }
    }
}

fn report(result: Result<(), Diagnostic>, cx: &mut App) {
    let previous = cx.default_global::<Writer>().last_error.take();
    let state = cx.default_global::<SettingsState>();
    if let Some(previous) = &previous {
        state.diagnostics.retain(|d| d != previous);
    }
    if let Err(problem) = result {
        tracing::warn!(problem = %problem, "settings not saved");
        state.diagnostics.push(problem.clone());
        cx.default_global::<Writer>().last_error = Some(problem);
    }
    cx.refresh_windows();
}

/// Reads the file at `configured` (resolving a symbolic link), applies `changes` and writes it
/// back atomically. Blocking: call it on a background thread only.
///
/// # Errors
/// A user-presentable diagnostic; the file is left unchanged.
pub fn save_blocking(configured: &Path, changes: &[SettingsChange]) -> Result<(), Diagnostic> {
    let _guard = SAVE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let file = resolve_settings_file(configured)
        .map_err(|problem| Diagnostic::new(format!("settings not saved: {}", problem.message())))?;
    if file == configured
        && let Some(dir) = configured.parent().filter(|d| !d.as_os_str().is_empty())
    {
        prepare_dir(dir)?;
    }
    let text = match read_limited(&file, max_settings_bytes()) {
        Ok(None) => String::new(),
        Ok(Some(bytes)) => String::from_utf8(bytes).map_err(|_| {
            Diagnostic::new("settings not saved: settings.toml is not valid UTF-8 any more")
        })?,
        Err(e) => {
            return Err(Diagnostic::new(format!(
                "settings not saved: settings.toml cannot be read ({e})"
            )));
        }
    };
    let updated = apply_changes(&text, changes)?;
    if updated == text {
        return Ok(());
    }
    write_atomic(&file, updated.as_bytes())
        .map_err(|e| Diagnostic::new(format!("settings not saved: {e}")))?;
    tracing::debug!(changes = changes.len(), "settings.toml saved");
    Ok(())
}

/// Creates a missing configuration directory (`0700`) or checks an existing real one; a
/// symbolically linked directory is used as it is.
fn prepare_dir(dir: &Path) -> Result<(), Diagnostic> {
    let is_link = match std::fs::symlink_metadata(dir) {
        Ok(meta) => meta.file_type().is_symlink(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => false,
        Err(e) => return Err(Diagnostic::new(format!("settings not saved: {e}"))),
    };
    if is_link {
        return Ok(());
    }
    ensure_private_dir(dir).map_err(|e| Diagnostic::new(format!("settings not saved: {e}")))
}

/// Applies `changes` to the settings text `text` (empty for a new file) and returns the new
/// text. Only the changed keys are touched; comments and unknown keys are kept.
///
/// # Errors
/// The text is not valid TOML, has a newer `schema_version`, or `[appearance]` /
/// `[keybindings]` is not a table.
pub fn apply_changes(text: &str, changes: &[SettingsChange]) -> Result<String, Diagnostic> {
    let mut doc: DocumentMut = text.parse().map_err(|_| {
        Diagnostic::new("settings not saved: settings.toml is not valid TOML any more")
    })?;
    match doc.get(SCHEMA_VERSION_KEY) {
        None => {
            doc.insert(
                SCHEMA_VERSION_KEY,
                Item::Value(Value::from(SETTINGS_SCHEMA_VERSION)),
            );
            // Keep it first in the file, before any key the file already has.
            doc.sort_values_by(|a, _, b, _| {
                (b.get() == SCHEMA_VERSION_KEY).cmp(&(a.get() == SCHEMA_VERSION_KEY))
            });
        }
        Some(item) => {
            if item
                .as_integer()
                .is_some_and(|v| v > SETTINGS_SCHEMA_VERSION)
            {
                return Err(Diagnostic::new(
                    "settings not saved: settings.toml was written by a newer NineSQL",
                ));
            }
        }
    }
    for change in changes {
        let (section, _) = change.key();
        let table = section_mut(&mut doc, section)?;
        match change {
            SettingsChange::Theme(theme) => set_value(
                table,
                "theme",
                Value::from(match theme {
                    ThemePreference::Light => "light",
                    ThemePreference::Dark => "dark",
                    ThemePreference::System => "system",
                }),
            ),
            SettingsChange::UiFontFamily(f) => {
                set_or_remove(table, "ui_font_family", f.as_deref().map(Value::from))
            }
            SettingsChange::MonoFontFamily(f) => {
                set_or_remove(table, "mono_font_family", f.as_deref().map(Value::from))
            }
            SettingsChange::UiFontSize(s) => set_or_remove(table, "ui_font_size", s.map(number)),
            SettingsChange::MonoFontSize(s) => {
                set_or_remove(table, "mono_font_size", s.map(number));
            }
            SettingsChange::Zoom(z) => set_value(table, "zoom", number(*z)),
            SettingsChange::Keybinding(action, keys) => set_binding(table, action, keys.as_deref()),
        }
    }
    Ok(doc.to_string())
}

/// The `[section]` table, created at the end of the file when missing.
fn section_mut<'d>(
    doc: &'d mut DocumentMut,
    section: &str,
) -> Result<&'d mut dyn TableLike, Diagnostic> {
    doc.entry(section)
        .or_insert_with(toml_edit::table)
        .as_table_like_mut()
        .ok_or_else(|| {
            Diagnostic::new(format!(
                "settings not saved: [{section}] in settings.toml is not a table"
            ))
        })
}

/// Sets `key`, keeping the comments and spacing around an existing value.
fn set_value(table: &mut dyn TableLike, key: &str, new: Value) {
    match table.get_mut(key) {
        Some(Item::Value(old)) => {
            let decor = old.decor().clone();
            *old = new;
            *old.decor_mut() = decor;
        }
        _ => {
            table.insert(key, Item::Value(new));
        }
    }
}

fn set_or_remove(table: &mut dyn TableLike, key: &str, new: Option<Value>) {
    match new {
        Some(new) => set_value(table, key, new),
        None => {
            table.remove(key);
        }
    }
}

/// Writes the `[keybindings]` entry of `action`, reusing the key already in the file (short or
/// full action name) if there is one.
fn set_binding(table: &mut dyn TableLike, action: &str, keys: Option<&[String]>) {
    let existing: Vec<String> = table
        .iter()
        .map(|(k, _)| k.to_owned())
        .filter(|k| actions::find(k).is_some_and(|spec| spec.name() == action))
        .collect();
    let Some(keys) = keys else {
        for k in &existing {
            table.remove(k);
        }
        return;
    };
    let value = match keys {
        [single] => Value::from(single.as_str()),
        many => Value::Array(many.iter().map(String::as_str).collect()),
    };
    match existing.split_first() {
        Some((first, rest)) => {
            set_value(table, first, value);
            for k in rest {
                table.remove(k);
            }
        }
        None => {
            table.insert(action, Item::Value(value));
        }
    }
}

/// A size or zoom value: an integer when whole, else rounded to two decimals.
fn number(x: f32) -> Value {
    let rounded = (f64::from(x) * 100.0).round() / 100.0;
    if rounded.fract() == 0.0 && rounded.abs() < 1e6 {
        #[allow(clippy::cast_possible_truncation)]
        Value::from(rounded as i64)
    } else {
        Value::from(rounded)
    }
}

#[cfg(test)]
mod tests {
    use super::{SettingsChange, apply_changes};
    use crate::settings::{ThemePreference, parse};

    const HAND_WRITTEN: &str = r#"# My NineSQL settings
schema_version = 1
future_option = true   # unknown, kept

[appearance]
# dark at night
theme = "dark"     # trailing comment
ui_font_size = 15
mono_font_size = "big"  # invalid, kept as is

[keybindings]
NewEditorTab = "ctrl-n"
NoSuchAction = "ctrl-x"

[editor]
tab_size = 4
"#;

    #[test]
    fn edits_only_the_changed_keys() {
        let out = apply_changes(
            HAND_WRITTEN,
            &[
                SettingsChange::Theme(ThemePreference::Light),
                SettingsChange::Zoom(2.0),
                SettingsChange::UiFontSize(Some(13.5)),
            ],
        );
        let Ok(out) = out else {
            panic!("{out:?}");
        };
        for kept in [
            "# My NineSQL settings",
            "future_option = true   # unknown, kept",
            "# dark at night",
            "theme = \"light\"     # trailing comment",
            "mono_font_size = \"big\"  # invalid, kept as is",
            "NoSuchAction = \"ctrl-x\"",
            "[editor]\ntab_size = 4",
            "ui_font_size = 13.5",
            "zoom = 2",
        ] {
            assert!(out.contains(kept), "missing {kept:?} in:\n{out}");
        }
        let (settings, _) = parse(&out);
        assert_eq!(settings.appearance.theme, ThemePreference::Light);
        assert_eq!(settings.appearance.zoom, 2.0);
    }

    #[test]
    fn keybindings_reuse_short_names_and_unbind() {
        let Ok(out) = apply_changes(
            HAND_WRITTEN,
            &[
                SettingsChange::Keybinding(
                    "ninesql::NewEditorTab",
                    Some(vec!["alt-t".into(), "ctrl-k t".into()]),
                ),
                SettingsChange::Keybinding("ninesql::ToggleNavigator", Some(vec![])),
                SettingsChange::Keybinding("ninesql::CloseTab", Some(vec!["ctrl-alt-w".into()])),
            ],
        ) else {
            panic!("apply failed");
        };
        assert!(
            out.contains("NewEditorTab = [\"alt-t\", \"ctrl-k t\"]"),
            "{out}"
        );
        assert!(out.contains("\"ninesql::ToggleNavigator\" = []"), "{out}");
        assert!(
            out.contains("\"ninesql::CloseTab\" = \"ctrl-alt-w\""),
            "{out}"
        );
        let (settings, _) = parse(&out);
        assert_eq!(
            settings.keybindings.get("ninesql::NewEditorTab"),
            Some(&["alt-t".to_owned(), "ctrl-k t".to_owned()][..])
        );

        let Ok(back) = apply_changes(
            &out,
            &[SettingsChange::Keybinding("ninesql::NewEditorTab", None)],
        ) else {
            panic!("apply failed");
        };
        assert!(!back.contains("NewEditorTab"), "{back}");
        assert!(back.contains("NoSuchAction"), "{back}");
    }

    #[test]
    fn new_file_gets_schema_version_first() {
        let Ok(out) = apply_changes(
            "",
            &[
                SettingsChange::Theme(ThemePreference::System),
                SettingsChange::MonoFontFamily(Some("JetBrains Mono".into())),
            ],
        ) else {
            panic!("apply failed");
        };
        assert!(out.starts_with("schema_version = 1\n"), "{out}");
        let (settings, diags) = parse(&out);
        assert!(diags.is_empty(), "{diags:?}");
        assert_eq!(settings.appearance.theme, ThemePreference::System);
        assert_eq!(
            settings.appearance.mono_font_family.as_deref(),
            Some("JetBrains Mono")
        );

        // A file without schema_version gets it before its other root keys.
        let Ok(out) = apply_changes("# c\nx = 1\n", &[SettingsChange::Zoom(-1.0)]) else {
            panic!("apply failed");
        };
        let version = out.find("schema_version");
        let x = out.find("x = 1");
        assert!(version.is_some() && version < x, "{out}");
        assert!(out.contains("# c"), "{out}");
    }

    #[test]
    fn removes_keys_set_to_default() {
        let Ok(out) = apply_changes(
            HAND_WRITTEN,
            &[
                SettingsChange::UiFontSize(None),
                SettingsChange::UiFontFamily(None),
            ],
        ) else {
            panic!("apply failed");
        };
        assert!(!out.contains("ui_font_size"), "{out}");
    }

    #[test]
    fn refuses_files_it_must_not_overwrite() {
        let theme = [SettingsChange::Theme(ThemePreference::Dark)];
        assert!(apply_changes("schema_version = 2\n", &theme).is_err());
        assert!(apply_changes("[appearance\n", &theme).is_err());
        assert!(apply_changes("appearance = 3\n", &theme).is_err());
        // Inline tables are edited in place.
        let Ok(out) = apply_changes("appearance = { theme = \"light\" }\n", &theme) else {
            panic!("apply failed");
        };
        assert!(out.contains("appearance = { theme = \"dark\" }"), "{out}");
    }
}
