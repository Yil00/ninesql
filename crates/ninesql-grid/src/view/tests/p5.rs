//! `TestAppContext` tests of the P5-A parts of the results view: view modes, panels,
//! client-side sort / filter through the key bindings, advanced copy.

use std::sync::Arc;

use gpui::{TestAppContext, VisualTestContext};
use ninesql_core::driver_api::{AnsiDialect, ResultItem};

use super::{batch, clipboard, header, item, open, select_cell, send, small_budget};
use crate::rows::{ClientFilter, FilterOp};
use crate::selection::GridSelection;
use crate::source::ExecutionEvent;
use crate::view::{ResultsView, SidePanel, ViewMode};
use crate::viewer::ViewerKind;

fn loaded(cx: &mut TestAppContext, rows: usize) -> (gpui::Entity<ResultsView>, VisualTestContext) {
    let (view, _, mut vcx) = open(cx, small_budget());
    send(
        &view,
        &mut vcx,
        vec![
            ExecutionEvent::Started,
            item(0, header()),
            item(0, ResultItem::Rows(batch(0, rows))),
        ],
    );
    (view, vcx)
}

#[gpui::test]
fn sort_and_filter_loaded_rows_through_key_bindings(cx: &mut TestAppContext) {
    let (view, mut vcx) = loaded(cx, 10);
    select_cell(&view, &mut vcx, 0, 0);
    vcx.simulate_keystrokes("alt-s d");
    let label = view.read_with(&vcx, |v, cx| v.rows_label(cx)).unwrap();
    assert!(label.starts_with("Loaded rows only"), "{label}");
    assert!(label.contains("sorted by id ↓"), "{label}");
    // Display row 0 is now id 9: copy follows the view.
    select_cell(&view, &mut vcx, 0, 0);
    vcx.simulate_keystrokes("ctrl-c");
    assert_eq!(clipboard(&vcx), "9");
    // Filter by the selected value, then clear.
    vcx.simulate_keystrokes("alt-s v");
    assert_eq!(view.read_with(&vcx, |v, cx| v.shown_rows(cx)), Some(1));
    vcx.simulate_keystrokes("alt-s c");
    assert_eq!(view.read_with(&vcx, |v, cx| v.shown_rows(cx)), Some(10));
    assert!(view.read_with(&vcx, |v, cx| v.rows_label(cx)).is_none());
}

#[gpui::test]
fn filter_bar_applies_on_enter(cx: &mut TestAppContext) {
    let (view, mut vcx) = loaded(cx, 10);
    select_cell(&view, &mut vcx, 0, 0);
    vcx.simulate_keystrokes("alt-s f");
    assert!(view.read_with(&vcx, |v, _| v.filter_bar_open()));
    vcx.simulate_input(">= 7");
    vcx.simulate_keystrokes("enter");
    assert!(!view.read_with(&vcx, |v, _| v.filter_bar_open()));
    assert_eq!(view.read_with(&vcx, |v, cx| v.shown_rows(cx)), Some(3));
    let label = view.read_with(&vcx, |v, cx| v.rows_label(cx)).unwrap();
    assert!(label.contains("id ≥ \"7\""), "{label}");
    assert!(label.contains("3 of 10 loaded rows shown"), "{label}");
}

#[gpui::test]
fn view_modes_and_record_navigation(cx: &mut TestAppContext) {
    let (view, mut vcx) = loaded(cx, 5);
    select_cell(&view, &mut vcx, 1, 0);
    vcx.simulate_keystrokes("alt-2");
    assert_eq!(view.read_with(&vcx, |v, _| v.mode()), ViewMode::Record);
    let fields = view.read_with(&vcx, |v, cx| v.record_fields(cx));
    assert_eq!(fields[0].text, "1");
    assert_eq!(fields[1].text, "n1");
    vcx.simulate_keystrokes("alt-down alt-down");
    let fields = view.read_with(&vcx, |v, cx| v.record_fields(cx));
    assert_eq!(fields[0].text, "3");
    assert!(fields[1].is_null);
    vcx.simulate_keystrokes("alt-up");
    assert_eq!(
        view.read_with(&vcx, |v, cx| v.record_fields(cx))[0].text,
        "2"
    );
    vcx.simulate_keystrokes("alt-3");
    assert_eq!(view.read_with(&vcx, |v, _| v.mode()), ViewMode::Text);
    let text = view.update(&mut vcx, |v, cx| v.text_view(cx)).unwrap();
    assert!(text.text.starts_with("id | name\n"), "{}", text.text);
    assert_eq!(text.rows, 5);
    vcx.simulate_keystrokes("alt-1");
    assert_eq!(view.read_with(&vcx, |v, _| v.mode()), ViewMode::Grid);
}

#[gpui::test]
fn panels_toggle_and_show_the_selection(cx: &mut TestAppContext) {
    let (view, mut vcx) = loaded(cx, 10);
    vcx.simulate_keystrokes("alt-v");
    assert_eq!(
        view.read_with(&vcx, |v, _| v.panel()),
        Some(SidePanel::Value)
    );
    select_cell(&view, &mut vcx, 4, 1);
    let i = view.update(&mut vcx, |v, cx| v.inspection(cx)).unwrap();
    assert_eq!(i.text, "n4");
    assert_eq!(i.viewers, vec![ViewerKind::Text]);
    vcx.simulate_keystrokes("alt-a");
    assert_eq!(
        view.read_with(&vcx, |v, _| v.panel()),
        Some(SidePanel::Aggregate)
    );
    vcx.simulate_keystrokes("ctrl-a");
    let a = view.update(&mut vcx, |v, cx| v.aggregates(cx)).unwrap();
    assert_eq!(a.rows, 10);
    assert_eq!(a.columns[0].sum_text.as_deref(), Some("45"));
    vcx.simulate_keystrokes("alt-m");
    assert_eq!(
        view.read_with(&vcx, |v, _| v.panel()),
        Some(SidePanel::Metadata)
    );
    let info = view.read_with(&vcx, |v, cx| v.column_info(cx));
    assert_eq!(info.len(), 2);
    assert_eq!(info[0].source, None, "unknown unless the owner says");
    vcx.simulate_keystrokes("alt-m");
    assert_eq!(view.read_with(&vcx, |v, _| v.panel()), None);
}

#[gpui::test]
fn advanced_copy_formats(cx: &mut TestAppContext) {
    let (view, mut vcx) = loaded(cx, 3);
    select_cell(&view, &mut vcx, 0, 0);
    vcx.simulate_keystrokes("shift-down shift-right");
    vcx.simulate_keystrokes("ctrl-alt-m");
    assert_eq!(
        clipboard(&vcx),
        "| id | name |\n| --- | --- |\n| 0 | n0 |\n| 1 | n1 |\n"
    );
    vcx.simulate_keystrokes("ctrl-alt-j");
    assert!(
        clipboard(&vcx).contains("\"name\": \"n1\""),
        "{}",
        clipboard(&vcx)
    );
    // SQL INSERT without a dialect: refused, said in the status line.
    vcx.simulate_keystrokes("ctrl-alt-i");
    let status = view
        .read_with(&vcx, |v, _| v.status().map(str::to_owned))
        .unwrap();
    assert!(status.contains("dialect"), "{status}");
    view.update(&mut vcx, |v, _| {
        v.set_dialect(Some(Arc::new(AnsiDialect)));
        v.set_source_table(Some(vec!["s".into(), "t".into()]));
    });
    vcx.simulate_keystrokes("ctrl-alt-i");
    assert_eq!(
        clipboard(&vcx),
        "INSERT INTO \"s\".\"t\" (\"id\", \"name\") VALUES (0, 'n0');\nINSERT INTO \"s\".\"t\" (\"id\", \"name\") VALUES (1, 'n1');\n"
    );
}

#[gpui::test]
fn client_filter_survives_new_pages(cx: &mut TestAppContext) {
    let (view, mut vcx) = loaded(cx, 10);
    view.update(&mut vcx, |v, cx| {
        v.add_client_filter(ClientFilter::on(0, FilterOp::Less, "3"), cx);
    });
    assert_eq!(view.read_with(&vcx, |v, cx| v.shown_rows(cx)), Some(3));
    view.update(&mut vcx, |v, cx| {
        v.select(GridSelection::All, cx);
    });
    send(
        &view,
        &mut vcx,
        vec![
            item(0, ResultItem::Rows(batch(10, 5))),
            ExecutionEvent::StatementFinished { statement: 0 },
        ],
    );
    // New rows are filtered too once the page is in.
    assert_eq!(view.read_with(&vcx, |v, cx| v.shown_rows(cx)), Some(3));
}
