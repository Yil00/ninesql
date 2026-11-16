//! Dialogs of table editing (P5-A): the production save confirmation with the exact script, the
//! DML preview, the cell editor, the "Insert row" form, the server-side filter with its history
//! and the foreign-key target picker. Every dialog is keyboard-reachable (Tab between fields,
//! Escape closes) and never shows a password.

use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{ActiveTheme as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use ninesql_core::{FilterHistoryEntry, SaveConfirmation, SavePlan};

use super::{ConfirmationGuard, SqlEditorTab};
use crate::connections::visuals::{ConnectionBadges, hsla};
use crate::script_view::{ScriptRows, script_view};

impl SqlEditorTab {
    /// The color of the save confirmation (the session's type, production's when the core says
    /// production and the session shows none) and the guard telling the tab when the dialog is
    /// gone.
    pub(super) fn save_confirmation_parts(
        &self,
        production: bool,
        cx: &App,
    ) -> (Hsla, Rc<dyn std::any::Any>) {
        let badges = self
            .session()
            .map(|s| ConnectionBadges::of_open_session(self.hub.read(cx), s));
        let color = match badges {
            Some(b) if b.production || !production => b.color,
            _ => hsla(ninesql_core::ConnectionType::production().color),
        };
        let guard: Rc<dyn std::any::Any> = Rc::new(ConfirmationGuard {
            generation: SAVE_CONFIRMATION_GENERATION,
            dropped: self.confirmation_dropped.clone(),
        });
        (color, guard)
    }
}

/// Generation the save confirmation's guard reports (never a run's).
pub(super) const SAVE_CONFIRMATION_GENERATION: u64 = u64::MAX;

/// The unchecked columns as shown: each name through [`crate::browse::display_name`].
#[must_use]
pub fn unchecked_columns_text(columns: &[String]) -> String {
    format!(
        "Concurrent changes to these columns are not detected: {}",
        columns
            .iter()
            .map(|c| crate::browse::display_name(c))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn counts_text(deletes: usize, updates: usize, inserts: usize) -> String {
    format!("{updates} updated, {inserts} inserted, {deletes} deleted")
}

/// The confirmation of a save on a production (or confirm-every-write) connection: the
/// connection in its type color, the table, the counts, the unchecked columns and the exact
/// script. "Save changes" saves again with the token; "Cancel", Escape or a dialog closed
/// otherwise saves nothing (the changes stay pending).
pub fn open_save_confirmation(
    tab: Entity<SqlEditorTab>,
    confirmation: SaveConfirmation,
    color: Hsla,
    guard: Rc<dyn std::any::Any>,
    window: &mut Window,
    cx: &mut App,
) {
    let decided = Rc::new(std::cell::Cell::new(false));
    let script = Rc::new(ScriptRows::new(&confirmation.script));
    let c = Rc::new(confirmation);
    window.open_dialog(cx, move |dialog, _window, cx| {
        let _guard = &guard;
        let muted = cx.theme().muted_foreground;
        let warn = cx.theme().warning;
        let ok = {
            let tab = tab.clone();
            let decided = decided.clone();
            Button::new("save-confirm")
                .danger()
                .label("Save changes")
                .on_click(move |_, window, cx| {
                    if decided.replace(true) {
                        return;
                    }
                    window.close_dialog(cx);
                    tab.update(cx, |t, cx| t.confirm_save(window, cx));
                })
        };
        let cancel = {
            let tab = tab.clone();
            let decided = decided.clone();
            Button::new("save-cancel")
                .label("Cancel")
                .on_click(move |_, window, cx| {
                    if decided.replace(true) {
                        return;
                    }
                    window.close_dialog(cx);
                    tab.update(cx, |t, cx| t.decline_save(cx));
                })
        };
        let body = v_flex()
            .gap_2()
            .text_sm()
            .border_2()
            .border_color(color)
            .p_2()
            .child(
                div()
                    .id("save-connection")
                    .px_1()
                    .bg(color)
                    .text_color(crate::connections::visuals::text_on(color))
                    .child(crate::browse::display_name(&c.connection_name)),
            )
            .child(div().child(format!(
                "Save {} changes to {}{}? They run in one transaction.",
                c.statements,
                crate::browse::display_message(&c.table),
                if c.production {
                    " on a production connection"
                } else {
                    ""
                }
            )))
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(counts_text(c.deletes, c.updates, c.inserts)),
            )
            .when(!c.unchecked_columns.is_empty(), |d| {
                d.child(
                    div()
                        .text_color(warn)
                        .child(unchecked_columns_text(&c.unchecked_columns)),
                )
            })
            .child(script_view("save-script", script.clone(), cx));
        dialog
            .title("Confirm save")
            .w(px(720.))
            .child(body)
            .footer(h_flex().gap_2().justify_end().child(cancel).child(ok))
    });
}

/// The DML preview ("Preview changes"): the statements the save would run, with "Save
/// changes" (which still asks on a production connection) and "Close".
pub fn open_preview(tab: Entity<SqlEditorTab>, plan: SavePlan, window: &mut Window, cx: &mut App) {
    let script = Rc::new(ScriptRows::new(&plan.script));
    let plan = Rc::new(plan);
    window.open_dialog(cx, move |dialog, _window, cx| {
        let warn = cx.theme().warning;
        let save = {
            let tab = tab.clone();
            Button::new("preview-save")
                .primary()
                .label("Save changes")
                .on_click(move |_, window, cx| {
                    window.close_dialog(cx);
                    tab.update(cx, |t, cx| t.save_changes(window, cx));
                })
        };
        let close = Button::new("preview-close")
            .label("Close")
            .on_click(|_, window, cx| window.close_dialog(cx));
        dialog
            .title(format!(
                "Changes to {}",
                crate::browse::display_message(&plan.table)
            ))
            .w(px(720.))
            .child(
                v_flex()
                    .gap_2()
                    .text_sm()
                    .child(counts_text(plan.deletes, plan.updates, plan.inserts))
                    .when(!plan.unchecked_columns.is_empty(), |d| {
                        d.child(
                            div()
                                .text_color(warn)
                                .child(unchecked_columns_text(&plan.unchecked_columns)),
                        )
                    })
                    .child(script_view("preview-script", script.clone(), cx)),
            )
            .footer(h_flex().gap_2().justify_end().child(close).child(save))
    });
}

/// The cell editor: the value as text (converted to the column's type by the core), "Set
/// NULL", "Cancel" and "Apply". A refused value keeps the dialog open with the reason.
pub fn open_cell_editor(
    tab: Entity<SqlEditorTab>,
    row: usize,
    col: usize,
    name: String,
    text: String,
    window: &mut Window,
    cx: &mut App,
) {
    let input = cx.new(|cx| InputState::new(window, cx).default_value(text));
    let error: Rc<std::cell::RefCell<Option<String>>> = Rc::new(std::cell::RefCell::new(None));
    input.update(cx, |s, cx| s.focus(window, cx));
    window.open_dialog(cx, move |dialog, _window, cx| {
        let danger = cx.theme().danger;
        let apply = {
            let tab = tab.clone();
            let input = input.clone();
            let error = error.clone();
            Button::new("cell-apply")
                .primary()
                .label("Apply")
                .on_click(move |_, window, cx| {
                    let value = input.read(cx).value().to_string();
                    let r = tab.update(cx, |t, cx| t.set_cell(row, col, Some(&value), cx));
                    match r {
                        Ok(()) => window.close_dialog(cx),
                        Err(e) => *error.borrow_mut() = Some(e),
                    }
                    window.refresh();
                })
        };
        let null = {
            let tab = tab.clone();
            let error = error.clone();
            Button::new("cell-null")
                .label("Set NULL")
                .on_click(move |_, window, cx| {
                    match tab.update(cx, |t, cx| t.set_cell(row, col, None, cx)) {
                        Ok(()) => window.close_dialog(cx),
                        Err(e) => *error.borrow_mut() = Some(e),
                    }
                    window.refresh();
                })
        };
        let cancel = Button::new("cell-cancel")
            .label("Cancel")
            .on_click(|_, window, cx| window.close_dialog(cx));
        dialog
            .title(format!("Edit {}", crate::browse::display_name(&name)))
            .w(px(520.))
            .child(
                v_flex()
                    .gap_2()
                    .text_sm()
                    .child(Input::new(&input))
                    .when_some(error.borrow().clone(), |d, e| {
                        d.child(div().text_color(danger).child(e))
                    }),
            )
            .footer(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(cancel)
                    .child(null)
                    .child(apply),
            )
    });
}

/// A field of the "Insert row" form: result field, column name, type, input.
type InsertField = (usize, String, String, Entity<InputState>);

/// The "Insert row" form: one field per editable column; an empty field takes the column's
/// default, "NULL" sets `NULL`.
pub fn open_insert_row(
    tab: Entity<SqlEditorTab>,
    columns: Vec<(usize, String, String)>,
    window: &mut Window,
    cx: &mut App,
) {
    let fields: Rc<Vec<InsertField>> = Rc::new(
        columns
            .into_iter()
            .map(|(f, n, t)| (f, n, t, cx.new(|cx| InputState::new(window, cx))))
            .collect(),
    );
    let nulls: Rc<std::cell::RefCell<Vec<bool>>> =
        Rc::new(std::cell::RefCell::new(vec![false; fields.len()]));
    let error: Rc<std::cell::RefCell<Option<String>>> = Rc::new(std::cell::RefCell::new(None));
    window.open_dialog(cx, move |dialog, _window, cx| {
        let muted = cx.theme().muted_foreground;
        let danger = cx.theme().danger;
        let rows = fields.iter().enumerate().map(|(i, (_, name, ty, input))| {
            let nulls_c = nulls.clone();
            let checked = nulls.borrow().get(i).copied().unwrap_or(false);
            h_flex()
                .gap_2()
                .child(div().w(px(160.)).child(crate::browse::display_name(name)))
                .child(
                    div()
                        .w(px(100.))
                        .text_xs()
                        .text_color(muted)
                        .child(ty.clone()),
                )
                .child(div().flex_1().child(Input::new(input)))
                .child(
                    Checkbox::new(("insert-null", i))
                        .label("NULL")
                        .checked(checked)
                        .on_click(move |v, window, _| {
                            if let Some(slot) = nulls_c.borrow_mut().get_mut(i) {
                                *slot = *v;
                            }
                            window.refresh();
                        }),
                )
        });
        let add = {
            let tab = tab.clone();
            let fields = fields.clone();
            let nulls = nulls.clone();
            let error = error.clone();
            Button::new("insert-add")
                .primary()
                .label("Add row")
                .on_click(move |_, window, cx| {
                    let values: Vec<(usize, Option<String>)> = fields
                        .iter()
                        .enumerate()
                        .filter_map(|(i, (f, _, _, input))| {
                            if nulls.borrow().get(i).copied().unwrap_or(false) {
                                return Some((*f, None));
                            }
                            let v = input.read(cx).value().to_string();
                            (!v.is_empty()).then_some((*f, Some(v)))
                        })
                        .collect();
                    match tab.update(cx, |t, cx| t.insert_row_with(values, cx)) {
                        Ok(_) => window.close_dialog(cx),
                        Err(e) => *error.borrow_mut() = Some(e),
                    }
                    window.refresh();
                })
        };
        let cancel = Button::new("insert-cancel")
            .label("Cancel")
            .on_click(|_, window, cx| window.close_dialog(cx));
        dialog
            .title("Insert row")
            .w(px(720.))
            .child(
                v_flex()
                    .gap_2()
                    .text_sm()
                    .child(div().text_color(muted).child(
                        "An empty field takes the column's default. Save to insert the row.",
                    ))
                    .children(rows)
                    .when_some(error.borrow().clone(), |d, e| {
                        d.child(div().text_color(danger).child(e))
                    }),
            )
            .footer(h_flex().gap_2().justify_end().child(cancel).child(add))
    });
}

/// The server-side filter: a `WHERE` expression (checked by the core: it must stay a read),
/// the table's filter history, "Clear" and "Apply".
pub fn open_filter_dialog(
    tab: Entity<SqlEditorTab>,
    current: Option<String>,
    history: Vec<FilterHistoryEntry>,
    window: &mut Window,
    cx: &mut App,
) {
    let input = cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder("price > 10 AND status = 'open'")
            .default_value(current.unwrap_or_default())
    });
    input.update(cx, |s, cx| s.focus(window, cx));
    let error: Rc<std::cell::RefCell<Option<String>>> = Rc::new(std::cell::RefCell::new(None));
    let history = Rc::new(history);
    window.open_dialog(cx, move |dialog, _window, cx| {
        let muted = cx.theme().muted_foreground;
        let danger = cx.theme().danger;
        let entries = history.iter().take(20).enumerate().map(|(i, e)| {
            let tab = tab.clone();
            let entry = e.clone();
            Button::new(("filter-history", i))
                .ghost()
                .label(history_label(e))
                .on_click(move |_, window, cx| {
                    window.close_dialog(cx);
                    tab.update(cx, |t, cx| t.apply_history_entry(&entry, window, cx));
                })
        });
        let apply = {
            let tab = tab.clone();
            let input = input.clone();
            let error = error.clone();
            Button::new("filter-apply")
                .primary()
                .label("Apply")
                .on_click(move |_, window, cx| {
                    let raw = input.read(cx).value().to_string();
                    match tab.update(cx, |t, cx| t.set_raw_where(Some(raw), window, cx)) {
                        Ok(()) => window.close_dialog(cx),
                        Err(e) => *error.borrow_mut() = Some(e),
                    }
                    window.refresh();
                })
        };
        let clear = {
            let tab = tab.clone();
            Button::new("filter-clear")
                .label("Clear filters")
                .on_click(move |_, window, cx| {
                    window.close_dialog(cx);
                    tab.update(cx, |t, cx| t.clear_server_filter(window, cx));
                })
        };
        let cancel = Button::new("filter-cancel")
            .label("Cancel")
            .on_click(|_, window, cx| window.close_dialog(cx));
        dialog
            .title("Filter table rows")
            .w(px(640.))
            .child(
                v_flex()
                    .gap_2()
                    .text_sm()
                    .child(div().text_color(muted).child(
                        "WHERE expression, run on the server. It must stay a read: no ';', UNION, ORDER BY or writes.",
                    ))
                    .child(Input::new(&input))
                    .when_some(error.borrow().clone(), |d, e| {
                        d.child(div().text_color(danger).child(e))
                    })
                    .when(!history.is_empty(), |d| {
                        d.child(div().text_color(muted).child("Recent filters"))
                            .child(v_flex().gap_1().children(entries))
                    }),
            )
            .footer(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(cancel)
                    .child(clear)
                    .child(apply),
            )
    });
}

/// One line describing a history entry (column names, operators and the raw text; values are
/// shown only as "value").
#[must_use]
pub fn history_label(e: &FilterHistoryEntry) -> String {
    filter_label(&e.filters, e.raw_where.as_deref(), &e.order_by)
}

/// [`history_label`] from its parts: column names through [`crate::browse::display_name`].
#[must_use]
pub fn filter_label(
    filters: &[ninesql_core::ColumnFilter],
    raw_where: Option<&str>,
    order_by: &[ninesql_core::SortKey],
) -> String {
    let mut parts: Vec<String> = filters
        .iter()
        .map(|f| {
            if f.op.takes_value() {
                format!(
                    "{} {} …",
                    crate::browse::display_name(&f.column),
                    f.op.as_str()
                )
            } else {
                format!(
                    "{} {}",
                    crate::browse::display_name(&f.column),
                    f.op.as_str()
                )
            }
        })
        .collect();
    if let Some(raw) = raw_where {
        parts.push(crate::browse::display_message(raw));
    }
    for k in order_by {
        parts.push(format!(
            "order by {} {}",
            crate::browse::display_name(&k.column),
            match k.direction {
                ninesql_core::SortDirection::Ascending => "asc",
                ninesql_core::SortDirection::Descending => "desc",
            }
        ));
    }
    if parts.is_empty() {
        "(no filter)".to_owned()
    } else {
        parts.join(" · ")
    }
}

/// What runs with a picked foreign-key target.
pub type OnTarget = Rc<dyn Fn(super::table::FkTarget, &mut Window, &mut App)>;

/// Picks one of several foreign-key targets.
pub fn open_fk_picker(
    title: &'static str,
    targets: Vec<super::table::FkTarget>,
    on_pick: OnTarget,
    window: &mut Window,
    cx: &mut App,
) {
    let targets = Rc::new(targets);
    window.open_dialog(cx, move |dialog, _window, _cx| {
        let rows = targets.iter().enumerate().map(|(i, t)| {
            let target = t.clone();
            let on_pick = on_pick.clone();
            Button::new(("fk-pick", i))
                .label(t.label.clone())
                .on_click(move |_, window, cx| {
                    window.close_dialog(cx);
                    on_pick(target.clone(), window, cx);
                })
        });
        dialog
            .title(title)
            .w(px(560.))
            .child(v_flex().gap_2().text_sm().children(rows))
    });
}
