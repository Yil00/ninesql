//! Key bindings: validation, defaults merged with the `[keybindings]` overrides of
//! settings.toml, installation into GPUI and display (F-WS-02).
//!
//! An override replaces every default binding of its action; an empty list unbinds it. Invalid
//! entries are reported as settings problems and the defaults of that action are kept.

use std::collections::BTreeMap;
use std::rc::Rc;

use gpui_kit::{App, Global, KeyBinding, KeyBindingContextPredicate};

use crate::actions::{ACTIONS, ActionSpec};
use crate::settings::Diagnostic;

const MODIFIERS: &[&str] = &[
    "ctrl",
    "alt",
    "shift",
    "cmd",
    "super",
    "win",
    "fn",
    "secondary",
];

const NAMED_KEYS: &[&str] = &[
    "enter",
    "escape",
    "tab",
    "space",
    "backspace",
    "delete",
    "insert",
    "home",
    "end",
    "pageup",
    "pagedown",
    "up",
    "down",
    "left",
    "right",
    "menu",
];

/// Checks one keystroke (`ctrl-shift-p`, `ctrl--`, `f5`).
fn validate_keystroke(stroke: &str) -> Result<(), String> {
    // A trailing `-` after a modifier is the minus key (`ctrl--`).
    let (mods, key) = match stroke.strip_suffix("--") {
        Some(prefix) => (prefix, "-"),
        None if stroke == "-" => ("", "-"),
        None => match stroke.rsplit_once('-') {
            Some((mods, key)) => (mods, key),
            None => ("", stroke),
        },
    };
    if !mods.is_empty() {
        for m in mods.split('-') {
            if !MODIFIERS.iter().any(|known| m.eq_ignore_ascii_case(known)) {
                return Err(format!("unknown modifier `{m}` in `{stroke}`"));
            }
        }
    }
    let lower = key.to_ascii_lowercase();
    let is_function_key = lower
        .strip_prefix('f')
        .and_then(|n| n.parse::<u8>().ok())
        .is_some_and(|n| (1..=24).contains(&n));
    let mut chars = key.chars();
    let single_char = matches!((chars.next(), chars.next()), (Some(c), None) if !c.is_control() && !c.is_whitespace());
    if key.is_empty() {
        return Err(format!("missing key in `{stroke}`"));
    }
    if NAMED_KEYS.contains(&lower.as_str()) || is_function_key || single_char {
        Ok(())
    } else {
        Err(format!("unknown key `{key}` in `{stroke}`"))
    }
}

/// Checks a binding: one or more keystrokes separated by spaces (`ctrl-k ctrl-t`).
///
/// # Errors
/// A message naming the offending keystroke.
pub fn validate_keys(keys: &str) -> Result<(), String> {
    let strokes: Vec<&str> = keys.split_whitespace().collect();
    if strokes.is_empty() {
        return Err("empty key binding".to_owned());
    }
    if strokes.len() > 4 {
        return Err(format!("`{keys}`: at most 4 keystrokes"));
    }
    strokes.into_iter().try_for_each(validate_keystroke)
}

/// Overrides from `[keybindings]`, already validated: action name → bindings (empty = unbound).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BindingOverrides(BTreeMap<&'static str, Vec<String>>);

impl BindingOverrides {
    /// Sets the bindings of `action` (full name).
    pub fn set(&mut self, action: &'static str, keys: Vec<String>) {
        self.0.insert(action, keys);
    }

    /// Bindings of `action` if overridden.
    #[must_use]
    pub fn get(&self, action: &str) -> Option<&[String]> {
        self.0.get(action).map(Vec::as_slice)
    }

    /// Number of overridden actions.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether nothing is overridden.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// The bindings in effect: defaults with overrides applied, per action, in registry order.
#[derive(Debug, Clone, Default)]
pub struct EffectiveKeymap {
    entries: Vec<(&'static ActionSpec, Vec<String>)>,
}

impl Global for EffectiveKeymap {}

impl EffectiveKeymap {
    /// Merges the registry defaults with `overrides`.
    #[must_use]
    pub fn resolve(overrides: &BindingOverrides) -> Self {
        let entries = ACTIONS
            .iter()
            .map(|spec| {
                let keys = match overrides.get(spec.name()) {
                    Some(keys) => keys.to_vec(),
                    None => spec.default_keys.iter().map(|k| (*k).to_owned()).collect(),
                };
                (spec, keys)
            })
            .collect();
        Self { entries }
    }

    /// Bindings of `action` (full name).
    #[must_use]
    pub fn keys_for(&self, action: &str) -> &[String] {
        self.entries
            .iter()
            .find(|(spec, _)| spec.name() == action)
            .map(|(_, keys)| keys.as_slice())
            .unwrap_or(&[])
    }

    /// The first binding of `action`, formatted for display.
    #[must_use]
    pub fn display_for(&self, action: &str) -> Option<String> {
        self.keys_for(action).first().map(|k| format_keys(k))
    }

    /// Every (action, binding) pair.
    pub fn iter(&self) -> impl Iterator<Item = (&'static ActionSpec, &str)> + '_ {
        self.entries
            .iter()
            .flat_map(|(spec, keys)| keys.iter().map(move |k| (*spec, k.as_str())))
    }

    /// Installs the bindings into GPUI and stores this keymap as a global. Bindings GPUI
    /// refuses are returned as problems, the others stay installed.
    pub fn install(self, cx: &mut App) -> Vec<Diagnostic> {
        let mut problems = Vec::new();
        let mut bindings = Vec::new();
        for (spec, keys) in self.iter() {
            let predicate = spec
                .scope
                .key_context()
                .and_then(|ctx| KeyBindingContextPredicate::parse(ctx).ok().map(Rc::new));
            match KeyBinding::load(keys, (spec.build)(), predicate, false, None, cx) {
                Ok(binding) => bindings.push(binding),
                Err(_) => problems.push(Diagnostic::new(format!(
                    "[keybindings] {}: `{keys}` is not a valid key binding",
                    spec.name()
                ))),
            }
        }
        cx.bind_keys(bindings);
        cx.set_global(self);
        problems
    }
}

/// Formats a binding for display: `ctrl-k ctrl-t` → `Ctrl+K Ctrl+T`.
#[must_use]
pub fn format_keys(keys: &str) -> String {
    keys.split_whitespace()
        .map(format_keystroke)
        .collect::<Vec<_>>()
        .join(" ")
}

fn format_keystroke(stroke: &str) -> String {
    let (mods, key) = match stroke.strip_suffix("--") {
        Some(prefix) => (prefix, "-"),
        None => stroke.rsplit_once('-').unwrap_or(("", stroke)),
    };
    let mut parts: Vec<String> = mods
        .split('-')
        .filter(|m| !m.is_empty())
        .map(|m| match m.to_ascii_lowercase().as_str() {
            "ctrl" | "secondary" => "Ctrl".to_owned(),
            "alt" => "Alt".to_owned(),
            "shift" => "Shift".to_owned(),
            "cmd" | "super" | "win" => "Super".to_owned(),
            "fn" => "Fn".to_owned(),
            other => other.to_owned(),
        })
        .collect();
    let key = match key.to_ascii_lowercase().as_str() {
        "pageup" => "PageUp".to_owned(),
        "pagedown" => "PageDown".to_owned(),
        "escape" => "Esc".to_owned(),
        "backspace" => "Backspace".to_owned(),
        k if k.len() == 1 => k.to_ascii_uppercase(),
        k => {
            let mut c = k.chars();
            c.next()
                .map(|first| first.to_ascii_uppercase().to_string() + c.as_str())
                .unwrap_or_default()
        }
    };
    parts.push(key);
    parts.join("+")
}

#[cfg(test)]
mod tests {
    use super::{BindingOverrides, EffectiveKeymap, format_keys, validate_keys};

    #[test]
    fn validates_keystrokes() {
        for ok in [
            "ctrl-t",
            "ctrl-shift-p",
            "f1",
            "F12",
            "ctrl--",
            "ctrl-=",
            "ctrl-\\",
            "ctrl-k ctrl-t",
            "alt-pageup",
            "super-space",
            "secondary-s",
            "-",
        ] {
            assert!(validate_keys(ok).is_ok(), "{ok} should be valid");
        }
        for bad in [
            "",
            "   ",
            "ctrl-",
            "hyper-x",
            "ctrl-foo",
            "f25",
            "a b c d e",
            "ctrl-shift",
        ] {
            assert!(validate_keys(bad).is_err(), "{bad:?} should be invalid");
        }
    }

    #[test]
    fn formats_for_display() {
        assert_eq!(format_keys("ctrl-shift-p"), "Ctrl+Shift+P");
        assert_eq!(format_keys("ctrl-k ctrl-t"), "Ctrl+K Ctrl+T");
        assert_eq!(format_keys("ctrl--"), "Ctrl+-");
        assert_eq!(format_keys("ctrl-pagedown"), "Ctrl+PageDown");
        assert_eq!(format_keys("f1"), "F1");
        assert_eq!(format_keys("ctrl-tab"), "Ctrl+Tab");
    }

    #[test]
    fn overrides_replace_or_unbind_defaults() {
        let mut overrides = BindingOverrides::default();
        overrides.set(
            "ninesql::NewEditorTab",
            vec!["ctrl-n".into(), "alt-t".into()],
        );
        overrides.set("ninesql::ToggleNavigator", vec![]);
        let keymap = EffectiveKeymap::resolve(&overrides);
        assert_eq!(
            keymap.keys_for("ninesql::NewEditorTab"),
            ["ctrl-n", "alt-t"]
        );
        assert!(keymap.keys_for("ninesql::ToggleNavigator").is_empty());
        assert_eq!(keymap.keys_for("ninesql::CloseTab"), ["ctrl-w"]);
        assert_eq!(
            keymap.display_for("ninesql::NewEditorTab").as_deref(),
            Some("Ctrl+N")
        );
        assert_eq!(keymap.display_for("ninesql::ToggleNavigator"), None);
        assert!(
            !keymap
                .iter()
                .any(|(spec, keys)| spec.name() == "ninesql::NewEditorTab" && keys == "ctrl-t"),
            "the default is replaced, not kept next to the override"
        );
    }
}
