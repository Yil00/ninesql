//! Command palette (F-WS-02): every registered action, grouped by category, with its effective
//! key binding, searchable by label and keywords.
//!
//! Items carry no GPUI action of their own: the palette is drawn in a dialog layer outside the
//! workspace element, so a dispatch from there would miss the workspace handlers. Confirming an
//! item closes the dialog and runs the action through [`Workspace::perform`] instead, the same
//! code path as its key binding.

use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::command::{Command, CommandGroup, CommandItem, CommandState};
use gpui_kit::component::{IndexPath, WindowExt as _, h_flex};
use gpui_kit::*;

use crate::actions::{ActionSpec, registry};
use crate::keymap::EffectiveKeymap;
use crate::workspace::Workspace;

/// One palette row.
#[derive(Debug, Clone)]
pub struct PaletteEntry {
    /// The action.
    pub spec: &'static ActionSpec,
    /// First effective binding, formatted (`Ctrl+Shift+P`), if any.
    pub keys: Option<String>,
}

/// Palette rows grouped by category, in registry order.
#[must_use]
pub fn grouped_entries(keymap: &EffectiveKeymap) -> Vec<(&'static str, Vec<PaletteEntry>)> {
    let mut groups: Vec<(&'static str, Vec<PaletteEntry>)> = Vec::new();
    for spec in registry().iter().filter(|s| s.in_palette) {
        let entry = PaletteEntry {
            spec,
            keys: keymap.display_for(spec.name()),
        };
        match groups.iter_mut().find(|(cat, _)| *cat == spec.category) {
            Some((_, entries)) => entries.push(entry),
            None => groups.push((spec.category, vec![entry])),
        }
    }
    groups
}

fn item(entry: &PaletteEntry) -> CommandItem {
    let label: SharedString = entry.spec.label.into();
    let keys = entry.keys.clone();
    let row_label = label.clone();
    CommandItem::new()
        .label(label)
        .keywords(entry.spec.keywords.iter().copied())
        .keywords([entry.spec.name()])
        .child(move |_window, cx| {
            h_flex()
                .w_full()
                .justify_between()
                .gap_4()
                .child(row_label.clone())
                .children(keys.clone().map(|k| {
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(k)
                }))
        })
}

/// Opens the palette over `window`; the chosen action runs on `workspace`.
pub fn open(workspace: WeakEntity<Workspace>, window: &mut Window, cx: &mut App) {
    let keymap = cx
        .try_global::<EffectiveKeymap>()
        .cloned()
        .unwrap_or_default();
    let groups = Rc::new(grouped_entries(&keymap));
    let state = cx.new(|cx| CommandState::new(window, cx));
    let first_render = Cell::new(true);
    window.open_dialog(cx, move |dialog, window, cx| {
        if first_render.replace(false) {
            state.update(cx, |state, cx| state.focus(window, cx));
        }
        let on_confirm = {
            let groups = groups.clone();
            let workspace = workspace.clone();
            move |ix: IndexPath, window: &mut Window, cx: &mut App| {
                let Some(entry) = groups.get(ix.section).and_then(|(_, e)| e.get(ix.row)) else {
                    return;
                };
                let action = (entry.spec.build)();
                window.close_dialog(cx);
                let _ = workspace.update(cx, |ws, cx| ws.perform(action.as_ref(), window, cx));
            }
        };
        let mut command = Command::new(&state)
            .placeholder("Type a command")
            .on_confirm(on_confirm);
        for (category, entries) in groups.iter() {
            command = command.group(
                CommandGroup::new()
                    .label(*category)
                    .items(entries.iter().map(item)),
            );
        }
        dialog.close_button(false).w(px(560.)).p_0().child(command)
    });
}

#[cfg(test)]
mod tests {
    use super::grouped_entries;
    use crate::actions::registry;
    use crate::keymap::{BindingOverrides, EffectiveKeymap};

    #[test]
    fn palette_lists_every_action_but_itself_with_bindings() {
        let keymap = EffectiveKeymap::resolve(&BindingOverrides::default());
        let groups = grouped_entries(&keymap);
        let listed: Vec<_> = groups.iter().flat_map(|(_, e)| e).collect();
        assert_eq!(listed.len(), registry().len() - 1);
        assert!(
            listed.iter().all(|e| e.keys.is_some()),
            "every entry shows a binding"
        );
        assert!(
            !listed
                .iter()
                .any(|e| e.spec.name() == "ninesql::CommandPalette")
        );
        let editor = groups.iter().find(|(c, _)| *c == "Editor");
        assert!(editor.is_some_and(|(_, e)| e[0].keys.as_deref() == Some("Ctrl+T")));
        // The SQL editor's and the grid's actions are listed with their own bindings.
        let find = |name: &str| listed.iter().find(|e| e.spec.name() == name);
        assert_eq!(
            find("ninesql_editor::ExecuteStatement").and_then(|e| e.keys.as_deref()),
            Some("Ctrl+Enter")
        );
        assert_eq!(
            find("ninesql_grid::FetchAllRows").and_then(|e| e.keys.as_deref()),
            Some("Alt+End")
        );
        assert!(groups.iter().any(|(c, _)| *c == "SQL editor"));
        assert!(groups.iter().any(|(c, _)| *c == "Results"));
    }

    #[test]
    fn palette_shows_overridden_and_unbound_actions() {
        let mut overrides = BindingOverrides::default();
        overrides.set("ninesql::NewEditorTab", vec!["alt-n".into()]);
        overrides.set("ninesql::CloseTab", vec![]);
        let groups = grouped_entries(&EffectiveKeymap::resolve(&overrides));
        let find = |name: &str| {
            groups
                .iter()
                .flat_map(|(_, e)| e)
                .find(|e| e.spec.name() == name)
                .cloned()
        };
        assert_eq!(
            find("ninesql::NewEditorTab").and_then(|e| e.keys),
            Some("Alt+N".to_owned())
        );
        let close = find("ninesql::CloseTab");
        assert!(close.is_some(), "an unbound action stays in the palette");
        assert!(close.and_then(|e| e.keys).is_none());
    }
}
