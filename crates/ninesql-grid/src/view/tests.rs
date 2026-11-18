//! `TestAppContext` tests of the results view (AGENTS.md: no pixel tests): result tabs, panes,
//! selection and copy through the key bindings, paging and the memory budget, truncated
//! columns, virtualization.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{Entity, TestAppContext, VisualTestContext};
use ninesql_core::driver_api::arrow_array::{Int64Array, RecordBatch};
use ninesql_core::driver_api::arrow_schema::{DataType, Field, Schema};
use ninesql_core::driver_api::{
    Notice, NoticeSeverity, ResultItem, ResultSetHeader, Truncation, TruncationReason,
};

use super::*;
use crate::actions::GRID_ACTIONS;
use crate::model::batch_bytes;
use crate::model::tests::{batch, schema, truncated_batch};
use crate::source::{FetchMode, FetchRequest};

#[derive(Default)]
struct Recorder {
    fetches: RefCell<Vec<FetchRequest>>,
    releases: RefCell<Vec<u32>>,
}

impl ResultSource for Recorder {
    fn fetch(&self, request: FetchRequest) {
        self.fetches.borrow_mut().push(request);
    }
    fn release(&self, statement: u32) {
        self.releases.borrow_mut().push(statement);
    }
}

fn init(cx: &mut TestAppContext) {
    cx.update(|cx| {
        gpui_kit::init(cx);
        crate::init(cx);
    });
}

fn open(
    cx: &mut TestAppContext,
    budget: GridBudget,
) -> (Entity<ResultsView>, Rc<Recorder>, VisualTestContext) {
    init(cx);
    let recorder = Rc::new(Recorder::default());
    let source = recorder.clone();
    let window = cx.add_window(move |_, cx| {
        let mut view = ResultsView::new(budget, cx);
        view.set_source(source);
        view
    });
    let view = window.root(cx).unwrap();
    let mut vcx = VisualTestContext::from_window(window.into(), cx);
    vcx.update(|window, cx| {
        let handle = view.focus_handle(cx);
        window.focus(&handle, cx);
    });
    vcx.run_until_parked();
    (view, recorder, vcx)
}

fn send(view: &Entity<ResultsView>, vcx: &mut VisualTestContext, events: Vec<ExecutionEvent>) {
    vcx.update(|window, cx| {
        view.update(cx, |v, cx| {
            for e in events {
                v.apply(e, window, cx);
            }
        });
    });
    // Focus follows the active table.
    vcx.update(|window, cx| {
        let handle = view.focus_handle(cx);
        window.focus(&handle, cx);
    });
    vcx.run_until_parked();
}

fn item(statement: u32, item: ResultItem) -> ExecutionEvent {
    ExecutionEvent::Item { statement, item }
}

fn header() -> ResultItem {
    ResultItem::ResultSet(ResultSetHeader::tabular(0, schema()))
}

fn clipboard(vcx: &VisualTestContext) -> String {
    vcx.read_from_clipboard()
        .and_then(|c| c.text())
        .unwrap_or_default()
}

fn select_cell(view: &Entity<ResultsView>, vcx: &mut VisualTestContext, row: usize, col: usize) {
    let table = view.read_with(vcx, |v, _| v.table(0).cloned()).unwrap();
    vcx.update(|_, cx| table.update(cx, |t, cx| t.set_selected_cell(row, col, cx)));
    vcx.run_until_parked();
}

fn small_budget() -> GridBudget {
    GridBudget {
        page_rows: 10,
        ..GridBudget::default()
    }
}

#[gpui::test]
fn every_action_has_a_working_default_binding(cx: &mut TestAppContext) {
    init(cx);
    cx.update(|cx| {
        let keymap = cx.key_bindings();
        let keymap = keymap.borrow();
        let names = cx.all_action_names();
        for spec in GRID_ACTIONS {
            assert!(
                names.contains(&spec.name()),
                "{} not registered",
                spec.name()
            );
            assert!(spec.name().starts_with("ninesql_grid::"));
            let action = (spec.build)();
            let bound = keymap.bindings_for_action(action.as_ref()).count();
            assert_eq!(bound, spec.default_keys.len(), "{}", spec.name());
            assert!(!spec.label.is_empty());
        }
        let grid_names = names
            .iter()
            .filter(|n| n.starts_with("ninesql_grid::"))
            .count();
        assert_eq!(
            grid_names,
            GRID_ACTIONS.len(),
            "an action is missing from the registry"
        );
    });
}

#[gpui::test]
fn result_tabs_and_panes(cx: &mut TestAppContext) {
    let (view, _, mut vcx) = open(cx, small_budget());
    send(
        &view,
        &mut vcx,
        vec![
            ExecutionEvent::Started,
            ExecutionEvent::StatementStarted { statement: 0 },
            item(0, header()),
            item(0, ResultItem::Rows(batch(0, 10))),
            ExecutionEvent::StatementFinished { statement: 0 },
            ExecutionEvent::StatementStarted { statement: 1 },
            item(1, ResultItem::UpdateCount(Some(3))),
            item(
                1,
                ResultItem::Notice(Notice::new(
                    NoticeSeverity::Info,
                    "relation exists, skipping",
                )),
            ),
            ExecutionEvent::StatementFinished { statement: 1 },
            ExecutionEvent::StatementStarted { statement: 2 },
            item(2, header()),
            item(2, ResultItem::Rows(batch(0, 2))),
            ExecutionEvent::StatementFinished { statement: 2 },
            ExecutionEvent::Finished,
        ],
    );
    view.read_with(&vcx, |v, _| {
        assert_eq!(v.results().tabs().len(), 2);
        assert_eq!(v.active_pane(), ActivePane::Result(0), "first result shown");
        assert_eq!(v.results().tabs()[1].label(true), "3 · Result");
        assert_eq!(
            v.status_line(0).unwrap().split(" · ").next(),
            Some("10 rows")
        );
    });
    vcx.simulate_keystrokes("alt-right");
    assert_eq!(
        view.read_with(&vcx, |v, _| v.active_pane()),
        ActivePane::Result(1)
    );
    vcx.simulate_keystrokes("alt-right");
    assert_eq!(
        view.read_with(&vcx, |v, _| v.active_pane()),
        ActivePane::UpdateCounts
    );
    vcx.simulate_keystrokes("alt-right");
    assert_eq!(
        view.read_with(&vcx, |v, _| v.active_pane()),
        ActivePane::Notices
    );
    vcx.simulate_keystrokes("alt-right");
    assert_eq!(
        view.read_with(&vcx, |v, _| v.active_pane()),
        ActivePane::Result(0)
    );
    vcx.simulate_keystrokes("alt-left");
    assert_eq!(
        view.read_with(&vcx, |v, _| v.active_pane()),
        ActivePane::Notices
    );
    // A new execution clears the tabs.
    send(&view, &mut vcx, vec![ExecutionEvent::Started]);
    view.read_with(&vcx, |v, _| {
        assert!(v.results().tabs().is_empty());
        assert!(v.table(0).is_none());
        assert_eq!(v.active_pane(), ActivePane::UpdateCounts);
    });
}

#[gpui::test]
fn copy_through_key_bindings(cx: &mut TestAppContext) {
    let (view, _, mut vcx) = open(cx, small_budget());
    send(
        &view,
        &mut vcx,
        vec![
            ExecutionEvent::Started,
            item(0, header()),
            item(0, ResultItem::Rows(batch(0, 10))),
        ],
    );
    select_cell(&view, &mut vcx, 2, 1);
    vcx.simulate_keystrokes("ctrl-c");
    assert_eq!(clipboard(&vcx), "n2");
    // shift + arrows extend from the anchor.
    vcx.simulate_keystrokes("shift-down shift-down shift-left");
    vcx.simulate_keystrokes("ctrl-c");
    assert_eq!(clipboard(&vcx), "2\tn2\n3\t\n4\tn4\n");
    vcx.simulate_keystrokes("ctrl-shift-c");
    assert_eq!(clipboard(&vcx), "id\tname\n2\tn2\n3\t\n4\tn4\n");
    vcx.simulate_keystrokes("ctrl-alt-c");
    assert_eq!(clipboard(&vcx), "id,name\n2,n2\n3,\n4,n4\n");
    vcx.simulate_keystrokes("ctrl-a ctrl-c");
    assert_eq!(clipboard(&vcx).lines().count(), 10);
    assert!(view.read_with(&vcx, |v, _| {
        v.status().unwrap().starts_with("Copied 10 rows")
    }));
    // A plain arrow (the table's own navigation) collapses the selection to one cell.
    select_cell(&view, &mut vcx, 5, 0);
    vcx.simulate_keystrokes("down ctrl-c");
    assert_eq!(clipboard(&vcx), "6");
}

#[gpui::test]
fn ctrl_c_copies_have_the_formula_guard(cx: &mut TestAppContext) {
    use ninesql_core::driver_api::arrow_array::StringArray;
    let (view, _, mut vcx) = open(cx, small_budget());
    let s = Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int64, true),
        Field::new("name", DataType::Utf8, true),
    ]));
    let rows = RecordBatch::try_new(
        s.clone(),
        vec![
            Arc::new(Int64Array::from(vec![-1, 2])),
            Arc::new(StringArray::from(vec!["=HYPERLINK(\"x\")", "+1"])),
        ],
    )
    .unwrap();
    send(
        &view,
        &mut vcx,
        vec![
            ExecutionEvent::Started,
            item(0, ResultItem::ResultSet(ResultSetHeader::tabular(0, s))),
            item(0, ResultItem::Rows(rows)),
        ],
    );
    select_cell(&view, &mut vcx, 0, 1);
    vcx.simulate_keystrokes("ctrl-c");
    assert_eq!(clipboard(&vcx), "\"'=HYPERLINK(\"\"x\"\")\"");
    vcx.simulate_keystrokes("ctrl-a ctrl-c");
    assert_eq!(clipboard(&vcx), "-1\t\"'=HYPERLINK(\"\"x\"\")\"\n2\t'+1\n");
    vcx.simulate_keystrokes("ctrl-shift-c");
    assert_eq!(
        clipboard(&vcx),
        "id\tname\n-1\t\"'=HYPERLINK(\"\"x\"\")\"\n2\t'+1\n"
    );
}

#[gpui::test]
fn reordered_columns_copy_in_display_order(cx: &mut TestAppContext) {
    let (view, _, mut vcx) = open(cx, small_budget());
    send(
        &view,
        &mut vcx,
        vec![
            ExecutionEvent::Started,
            item(0, header()),
            item(0, ResultItem::Rows(batch(0, 10))),
        ],
    );
    let table = view.read_with(&vcx, |v, _| v.table(0).cloned()).unwrap();
    vcx.update(|window, cx| {
        table.update(cx, |t, cx| t.delegate_mut().move_column(1, 0, window, cx));
    });
    select_cell(&view, &mut vcx, 0, 0);
    vcx.simulate_keystrokes("shift-right ctrl-shift-c");
    assert_eq!(clipboard(&vcx), "name\tid\nn0\t0\n");
}

#[gpui::test]
fn fetch_next_page_and_fetch_all(cx: &mut TestAppContext) {
    let (view, recorder, mut vcx) = open(cx, small_budget());
    send(
        &view,
        &mut vcx,
        vec![ExecutionEvent::Started, item(0, header())],
    );
    // First page in flight: nothing to ask for yet.
    vcx.simulate_keystrokes("alt-pagedown");
    assert!(recorder.fetches.borrow().is_empty());
    send(
        &view,
        &mut vcx,
        vec![item(0, ResultItem::Rows(batch(0, 10)))],
    );
    vcx.simulate_keystrokes("alt-pagedown");
    assert_eq!(
        recorder.fetches.borrow().as_slice(),
        &[FetchRequest {
            statement: 0,
            result_set: 0,
            mode: FetchMode::Page { rows: 10 }
        }]
    );
    vcx.simulate_keystrokes("alt-pagedown");
    assert_eq!(recorder.fetches.borrow().len(), 1, "one page at a time");
    send(
        &view,
        &mut vcx,
        vec![item(0, ResultItem::Rows(batch(10, 10)))],
    );
    // Twenty rows fit in the window, so the table's load-more path (rows near the end) may
    // already have asked for the next page; otherwise ask through the delegate as scrolling does.
    let table = view.read_with(&vcx, |v, _| v.table(0).cloned()).unwrap();
    if vcx.update(|_, cx| table.read(cx).delegate().has_more(cx)) {
        vcx.update(|window, cx| table.update(cx, |t, cx| t.delegate_mut().load_more(window, cx)));
    }
    assert_eq!(recorder.fetches.borrow().len(), 2);
    assert!(
        !vcx.update(|_, cx| table.read(cx).delegate().has_more(cx)),
        "page in flight"
    );
    send(
        &view,
        &mut vcx,
        vec![item(0, ResultItem::Rows(batch(20, 10)))],
    );
    vcx.simulate_keystrokes("alt-end");
    let n = recorder.fetches.borrow().len();
    assert_eq!(recorder.fetches.borrow()[n - 1].mode, FetchMode::All);
    send(
        &view,
        &mut vcx,
        vec![
            item(0, ResultItem::Rows(batch(30, 10))),
            ExecutionEvent::StatementFinished { statement: 0 },
        ],
    );
    vcx.simulate_keystrokes("alt-end alt-pagedown");
    assert_eq!(
        recorder.fetches.borrow().len(),
        n,
        "complete: no more fetches"
    );
    assert!(!vcx.update(|_, cx| table.read(cx).delegate().has_more(cx)));
    view.read_with(&vcx, |v, _| {
        assert!(v.banner(0).is_none());
        assert!(v.status_line(0).unwrap().ends_with("all rows"));
    });
}

#[gpui::test]
fn truncated_banner_at_the_driver_budget(cx: &mut TestAppContext) {
    let (view, _, mut vcx) = open(cx, small_budget());
    send(
        &view,
        &mut vcx,
        vec![
            ExecutionEvent::Started,
            item(0, header()),
            item(0, ResultItem::Rows(batch(0, 10))),
            item(
                0,
                ResultItem::Truncated(Truncation::new(TruncationReason::MaxBytes, 10, 4096)),
            ),
            ExecutionEvent::StatementFinished { statement: 0 },
        ],
    );
    let banner = view.read_with(&vcx, |v, _| v.banner(0)).unwrap();
    assert!(
        banner.starts_with("Truncated: memory budget reached."),
        "{banner}"
    );
}

#[gpui::test]
fn grid_budget_releases_the_stream(cx: &mut TestAppContext) {
    let one = batch_bytes(&batch(0, 10));
    let budget = GridBudget {
        max_bytes: one * 2,
        page_rows: 10,
        max_notices: 100,
    };
    let (view, recorder, mut vcx) = open(cx, budget);
    send(
        &view,
        &mut vcx,
        vec![
            ExecutionEvent::Started,
            item(0, header()),
            item(0, ResultItem::Rows(batch(0, 10))),
        ],
    );
    vcx.simulate_keystrokes("alt-end");
    send(
        &view,
        &mut vcx,
        vec![
            item(0, ResultItem::Rows(batch(10, 10))),
            item(0, ResultItem::Rows(batch(20, 10))),
        ],
    );
    assert_eq!(recorder.releases.borrow().as_slice(), &[0]);
    view.read_with(&vcx, |v, _| {
        assert!(v.results().bytes() <= budget.max_bytes);
        assert!(
            v.banner(0)
                .unwrap()
                .starts_with("Truncated: memory budget reached.")
        );
        assert!(v.table(0).is_some());
    });
}

#[gpui::test]
fn truncated_columns_are_marked(cx: &mut TestAppContext) {
    let (view, _, mut vcx) = open(cx, small_budget());
    send(
        &view,
        &mut vcx,
        vec![
            ExecutionEvent::Started,
            item(0, header()),
            item(0, ResultItem::Rows(truncated_batch(0, 10))),
        ],
    );
    view.read_with(&vcx, |v, cx| {
        let table = v.table(0).unwrap().read(cx);
        let model = table.delegate().model().borrow();
        assert!(!model.columns()[1].editable());
        assert!(model.columns()[0].editable());
        assert!(v.banner(0).unwrap().contains("cut"));
    });
    select_cell(&view, &mut vcx, 0, 1);
    vcx.simulate_keystrokes("ctrl-c");
    let status = view.read_with(&vcx, |v, _| v.status().unwrap().to_owned());
    assert!(status.contains("cut by the value size limit"), "{status}");
}

#[gpui::test]
fn only_visible_rows_are_built_from_a_million(cx: &mut TestAppContext) {
    let (view, _, mut vcx) = open(cx, GridBudget::default());
    let s = Arc::new(Schema::new(vec![Field::new("v", DataType::Int64, false)]));
    let mut events = vec![
        ExecutionEvent::Started,
        item(
            0,
            ResultItem::ResultSet(ResultSetHeader::tabular(0, s.clone())),
        ),
    ];
    for b in 0..10 {
        let start = b * 100_000;
        events.push(item(
            0,
            ResultItem::Rows(
                RecordBatch::try_new(
                    s.clone(),
                    vec![Arc::new(Int64Array::from_iter_values(
                        start..start + 100_000,
                    ))],
                )
                .unwrap(),
            ),
        ));
    }
    send(&view, &mut vcx, events);
    let table = view.read_with(&vcx, |v, _| v.table(0).cloned()).unwrap();
    vcx.update(|_, cx| {
        let t = table.read(cx);
        assert_eq!(t.delegate().model().borrow().row_count(), 1_000_000);
        let visible = t.delegate().visible_rows();
        assert!(
            !visible.is_empty() && visible.len() < 200,
            "visible rows: {visible:?}"
        );
        assert_eq!(visible.start, 0);
    });
    // Jump to the end: the window follows, still small.
    vcx.update(|_, cx| table.update(cx, |t, cx| t.scroll_to_row(999_990, cx)));
    vcx.run_until_parked();
    vcx.update(|_, cx| {
        let visible = table.read(cx).delegate().visible_rows();
        assert!(visible.len() < 200, "visible rows: {visible:?}");
        assert!(
            visible.start > 999_000 && visible.end <= 1_000_000,
            "{visible:?}"
        );
    });
}

#[gpui::test]
fn auto_size_columns(cx: &mut TestAppContext) {
    let (view, _, mut vcx) = open(cx, small_budget());
    send(
        &view,
        &mut vcx,
        vec![
            ExecutionEvent::Started,
            item(0, header()),
            item(0, ResultItem::Rows(batch(0, 10))),
        ],
    );
    let table = view.read_with(&vcx, |v, _| v.table(0).cloned()).unwrap();
    let before = vcx.update(|_, cx| table.read(cx).col_width(1).unwrap());
    vcx.simulate_keystrokes("alt-shift-r");
    let after = vcx.update(|_, cx| table.read(cx).col_width(1).unwrap());
    assert!(after < before, "{after:?} < {before:?}");
    assert_eq!(
        vcx.update(|_, cx| table.read(cx).delegate().content_chars(1)),
        4
    );
    // "Fit column" needs a selected column.
    select_cell(&view, &mut vcx, 0, 0);
    vcx.simulate_keystrokes("alt-r");
    let id_width = vcx.update(|_, cx| table.read(cx).col_width(0).unwrap());
    assert!(id_width < px(100.));
}

#[gpui::test]
fn documents_tab_is_honest(cx: &mut TestAppContext) {
    let (view, _, mut vcx) = open(cx, small_budget());
    send(
        &view,
        &mut vcx,
        vec![
            ExecutionEvent::Started,
            item(0, ResultItem::ResultSet(ResultSetHeader::documents(0))),
            item(0, ResultItem::Documents(vec![])),
        ],
    );
    view.read_with(&vcx, |v, _| {
        assert!(v.table(0).is_none());
        assert!(
            v.banner(0)
                .unwrap()
                .contains("document viewer is not available")
        );
        assert_eq!(v.active_pane(), ActivePane::Result(0));
    });
    // Copy with no table does nothing.
    vcx.simulate_keystrokes("ctrl-c");
    assert!(view.read_with(&vcx, |v, _| v.status().is_none()));
}

#[gpui::test]
fn tables_are_built_when_first_shown_and_many_result_sets_stay_bounded(cx: &mut TestAppContext) {
    let (view, recorder, mut vcx) = open(cx, small_budget());
    let mut events = vec![
        ExecutionEvent::Started,
        ExecutionEvent::StatementStarted { statement: 0 },
    ];
    for i in 0..100_000u32 {
        events.push(item(
            0,
            ResultItem::ResultSet(ResultSetHeader::tabular(i, schema())),
        ));
    }
    events.push(ExecutionEvent::StatementFinished { statement: 0 });
    send(&view, &mut vcx, events);
    view.read_with(&vcx, |v, _| {
        assert_eq!(v.results().tabs().len(), crate::execution::MAX_RESULT_TABS);
        assert_eq!(v.built_tables(), 1, "only the shown tab has a table");
        assert!(v.results().bytes() <= v.results().budget().max_bytes);
        let banner = v.results().limit_banner().unwrap();
        assert!(banner.starts_with("Results limit reached: 99000 more result sets were not kept"));
    });
    assert_eq!(recorder.releases.borrow().as_slice(), &[0]);
    vcx.simulate_keystrokes("alt-right");
    view.read_with(&vcx, |v, _| {
        assert_eq!(v.active_pane(), ActivePane::Result(1));
        assert_eq!(v.built_tables(), 2);
        assert!(v.table(1).is_some());
    });
}

#[gpui::test]
fn an_idle_close_says_so_and_disables_fetching(cx: &mut TestAppContext) {
    let (view, recorder, mut vcx) = open(cx, small_budget());
    send(
        &view,
        &mut vcx,
        vec![
            ExecutionEvent::Started,
            ExecutionEvent::StatementStarted { statement: 0 },
            item(0, header()),
            item(0, ResultItem::Rows(batch(0, 10))),
            ExecutionEvent::ResultClosedIdle {
                statement: 0,
                rows: 10,
                after: std::time::Duration::from_secs(240),
            },
            ExecutionEvent::StatementClosed { statement: 0 },
            ExecutionEvent::Finished,
        ],
    );
    view.read_with(&vcx, |v, _| {
        assert_eq!(
            v.banner(0).unwrap(),
            "Result closed after 4 min idle: the 10 rows already loaded are kept. Run the \
             statement again to read more."
        );
        assert!(v.status_line(0).unwrap().ends_with("closed after idle"));
    });
    // "Fetch next page" and "Fetch all" do nothing (I8).
    vcx.simulate_keystrokes("alt-pagedown alt-end");
    assert!(recorder.fetches.borrow().is_empty());
    assert_eq!(super::idle_duration_text(45), "45 s");
}

mod p5;
