//! "Insert snippet" (F-SQL-10): a picker over the project's snippets (`snippets.json`, read off
//! the UI thread by `SnippetStore::load`), then a prompt for the snippet's placeholders, then
//! the expansion inserted at the cursor of the active SQL editor.
//!
//! An expansion is editor text: it is never run by the snippet code; the user reads it and runs
//! it like any other statement, with its classification and confirmations (I3). Values are
//! inserted as typed, and the editor sanitizes them like typed text.

use std::collections::HashMap;
use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{ActiveTheme as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use ninesql_core::{Expansion, Placeholder, Snippet, expand};

/// The placeholders the prompt asks for (the `${cursor}` marker is not one).
#[must_use]
pub fn prompted_placeholders(snippet: &Snippet) -> Vec<Placeholder> {
    let mut seen = std::collections::HashSet::new();
    snippet
        .placeholders()
        .into_iter()
        .filter(|p| p.name != "cursor" && seen.insert(p.name.clone()))
        .collect()
}

/// What runs with a picked snippet.
pub type OnPick = Rc<dyn Fn(Snippet, &mut Window, &mut App)>;
/// What runs with the expansion.
pub type OnExpand = Rc<dyn Fn(Expansion, &mut Window, &mut App)>;

/// The snippet picker: one button per snippet (name and description).
pub fn open_snippet_picker(
    snippets: Vec<Snippet>,
    on_pick: OnPick,
    window: &mut Window,
    cx: &mut App,
) {
    let snippets = Rc::new(snippets);
    window.open_dialog(cx, move |dialog, _window, cx| {
        let muted = cx.theme().muted_foreground;
        let rows = snippets.iter().enumerate().map(|(i, s)| {
            let snippet = s.clone();
            let on_pick = on_pick.clone();
            let label = match &s.description {
                Some(d) => format!(
                    "{} – {}",
                    crate::browse::display_name(&s.name),
                    crate::browse::display_name(d)
                ),
                None => crate::browse::display_name(&s.name),
            };
            Button::new(("snippet-pick", i))
                .label(label)
                .on_click(move |_, window, cx| {
                    window.close_dialog(cx);
                    on_pick(snippet.clone(), window, cx);
                })
        });
        dialog.title("Insert snippet").w(px(520.)).child(
            v_flex()
                .gap_2()
                .text_sm()
                .when(snippets.is_empty(), |d| {
                    d.child(div().text_color(muted).child(
                        "No snippet for this engine yet: snippets are kept per project in snippets.json.",
                    ))
                })
                .children(rows),
        )
    });
}

/// Asks for the placeholders of `snippet` (each prefilled with its default), then calls
/// `on_expand` with the expansion. A snippet without placeholders expands at once.
pub fn open_placeholder_prompt(
    snippet: Snippet,
    on_expand: OnExpand,
    window: &mut Window,
    cx: &mut App,
) {
    let placeholders = prompted_placeholders(&snippet);
    if placeholders.is_empty() {
        on_expand(expand(&snippet, &HashMap::new()), window, cx);
        return;
    }
    let inputs: Rc<Vec<(String, Entity<InputState>)>> = Rc::new(
        placeholders
            .iter()
            .map(|p| {
                let default = p.default.clone().unwrap_or_default();
                let input = cx.new(|cx| InputState::new(window, cx).default_value(default));
                (p.name.clone(), input)
            })
            .collect(),
    );
    let snippet = Rc::new(snippet);
    window.open_dialog(cx, move |dialog, _window, _cx| {
        let fields = inputs.iter().map(|(name, input)| {
            h_flex()
                .gap_2()
                .child(div().w(px(140.)).child(crate::browse::display_name(name)))
                .child(div().flex_1().child(Input::new(input)))
        });
        let insert = {
            let inputs = inputs.clone();
            let snippet = snippet.clone();
            let on_expand = on_expand.clone();
            Button::new("snippet-insert")
                .primary()
                .label("Insert")
                .on_click(move |_, window, cx| {
                    let values: HashMap<String, String> = inputs
                        .iter()
                        .map(|(n, i)| (n.clone(), i.read(cx).value().to_string()))
                        .collect();
                    window.close_dialog(cx);
                    on_expand(expand(&snippet, &values), window, cx);
                })
        };
        dialog
            .title(format!(
                "Snippet: {}",
                crate::browse::display_name(&snippet.name)
            ))
            .w(px(520.))
            .child(
                v_flex()
                    .gap_2()
                    .text_sm()
                    .child(
                        "Values are inserted as typed. Nothing runs until you run the statement.",
                    )
                    .children(fields),
            )
            .footer(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("snippet-cancel")
                            .label("Cancel")
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(insert),
            )
    });
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use ninesql_core::{Snippet, expand};

    use super::prompted_placeholders;

    #[test]
    fn cursor_and_repeated_placeholders_are_asked_once() {
        let s = Snippet::new(
            "sel",
            "SELECT ${cols:*} FROM ${table} WHERE ${table}.id = ${cursor}",
        );
        let names: Vec<String> = prompted_placeholders(&s)
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(names, vec!["cols".to_owned(), "table".to_owned()]);
        let mut values = HashMap::new();
        values.insert("table".to_owned(), "t".to_owned());
        let e = expand(&s, &values);
        assert_eq!(e.text, "SELECT * FROM t WHERE t.id = ");
        assert_eq!(e.cursor, Some(e.text.len()));
    }
}
