//! `TestAppContext` tests of the editor view (AGENTS.md: no pixel tests): key bindings reach the
//! editor's actions over gpui-component's own, commit-path hygiene, execution requests, the
//! parameters prompt and error positions.

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    ClipboardItem, Entity, EntityInputHandler as _, Focusable as _, TestAppContext,
    VisualTestContext,
};
use gpui_kit::component::input::Paste;
use ninesql_core::driver_api::{AnsiDialect, Param};

use crate::actions::EDITOR_ACTIONS;
use crate::execution::{
    ErrorLocation, ExecutionKind, ExecutionRequest, ExecutionSink, RequestId, StatementError,
};
use crate::split::DialectSplitter;
use crate::view::{EditorSetup, SqlEditor};

#[derive(Default)]
struct RecordingSink {
    requests: RefCell<Vec<ExecutionRequest>>,
    cancelled: RefCell<Vec<RequestId>>,
}

impl ExecutionSink for RecordingSink {
    fn submit(&self, request: ExecutionRequest) {
        self.requests.borrow_mut().push(request);
    }
    fn cancel(&self, id: RequestId) {
        self.cancelled.borrow_mut().push(id);
    }
}

fn init(cx: &mut TestAppContext) {
    cx.update(|cx| {
        gpui_kit::init(cx);
        crate::init(cx);
    });
}

fn setup() -> EditorSetup {
    EditorSetup::new(Rc::new(DialectSplitter::new(Arc::new(AnsiDialect))))
}

fn open(cx: &mut TestAppContext) -> (Entity<SqlEditor>, Rc<RecordingSink>, &mut VisualTestContext) {
    init(cx);
    let sink = Rc::new(RecordingSink::default());
    let sink_for_view = sink.clone();
    let (view, vcx) = cx.add_window_view(move |window, cx| {
        let mut editor = SqlEditor::new(setup(), window, cx);
        editor.set_sink(sink_for_view);
        editor
    });
    view.update_in(vcx, |v, window, cx| {
        let focus = v.focus_handle(cx);
        window.focus(&focus, cx);
    });
    vcx.run_until_parked();
    (view, sink, vcx)
}

fn set_text(view: &Entity<SqlEditor>, vcx: &mut VisualTestContext, text: &str, cursor: usize) {
    view.update_in(vcx, |v, window, cx| {
        v.set_text(text, window, cx);
        v.editor()
            .update(cx, |s, cx| s.set_selected_range(cursor..cursor, cx));
    });
    vcx.run_until_parked();
}

fn text(view: &Entity<SqlEditor>, vcx: &mut VisualTestContext) -> String {
    view.read_with(vcx, |v, cx| v.text(cx))
}

fn selected(view: &Entity<SqlEditor>, vcx: &mut VisualTestContext) -> std::ops::Range<usize> {
    view.read_with(vcx, |v, cx| v.editor().read(cx).selected_range())
}

fn diagnostics(view: &Entity<SqlEditor>, vcx: &mut VisualTestContext) -> usize {
    view.read_with(vcx, |v, cx| {
        v.editor().read(cx).diagnostics().map_or(0, |d| d.len())
    })
}

#[gpui::test]
fn every_action_is_registered_with_a_unique_default_binding(cx: &mut TestAppContext) {
    init(cx);
    cx.update(|cx| {
        let names = cx.all_action_names();
        let mut keys = HashSet::new();
        for spec in EDITOR_ACTIONS {
            assert!(
                names.contains(&spec.name()),
                "{} not registered",
                spec.name()
            );
            assert!(spec.name().starts_with("ninesql_editor::"));
            assert!(!spec.default_keys.is_empty());
            for key in spec.default_keys {
                assert!(keys.insert(*key), "{key} bound twice");
            }
            let action = (spec.build)();
            let bound = cx
                .key_bindings()
                .borrow()
                .bindings_for_action(action.as_ref())
                .next()
                .is_some();
            assert!(bound, "{} has no binding", spec.name());
        }
    });
}

#[gpui::test]
fn ctrl_enter_runs_the_statement_instead_of_inserting_a_newline(cx: &mut TestAppContext) {
    let (view, sink, vcx) = open(cx);
    set_text(&view, vcx, "select 1;\nselect 2;\n\nselect 3", 12);
    vcx.simulate_keystrokes("ctrl-enter");
    vcx.run_until_parked();
    assert_eq!(text(&view, vcx), "select 1;\nselect 2;\n\nselect 3");
    let requests = sink.requests.borrow();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].kind, ExecutionKind::Statement);
    assert_eq!(requests[0].items.len(), 1);
    assert_eq!(requests[0].items[0].text, "select 2");
    assert_eq!(requests[0].items[0].span, 10..18);
    assert_eq!(
        view.read_with(vcx, |v, _| v.running()),
        Some(requests[0].id)
    );
}

#[gpui::test]
fn alt_x_runs_the_script_and_a_blank_line_runs_nothing(cx: &mut TestAppContext) {
    let (view, sink, vcx) = open(cx);
    set_text(&view, vcx, "select 1;\nselect 2;\n\nselect 3", 20);
    vcx.simulate_keystrokes("ctrl-enter");
    vcx.run_until_parked();
    assert!(sink.requests.borrow().is_empty());
    assert_eq!(
        view.read_with(vcx, |v, _| v.notice().map(ToString::to_string)),
        Some("nothing to run here".to_owned())
    );
    vcx.simulate_keystrokes("alt-x");
    vcx.run_until_parked();
    let requests = sink.requests.borrow();
    let texts: Vec<&str> = requests[0].items.iter().map(|i| i.text.as_str()).collect();
    assert_eq!(texts, ["select 1", "select 2", "select 3"]);
    assert_eq!(requests[0].kind, ExecutionKind::Script);
}

#[gpui::test]
fn selection_runs_only_the_selected_statements(cx: &mut TestAppContext) {
    let (view, sink, vcx) = open(cx);
    set_text(&view, vcx, "select 1;\nselect 2;\nselect 3", 0);
    view.update_in(vcx, |v, _, cx| {
        v.editor()
            .update(cx, |s, cx| s.set_selected_range(10..28, cx));
    });
    vcx.simulate_keystrokes("ctrl-shift-enter");
    vcx.run_until_parked();
    let requests = sink.requests.borrow();
    let texts: Vec<&str> = requests[0].items.iter().map(|i| i.text.as_str()).collect();
    assert_eq!(texts, ["select 2", "select 3"]);
    assert_eq!(requests[0].kind, ExecutionKind::Selection);
}

#[gpui::test]
fn parameters_are_prompted_bound_and_not_kept(cx: &mut TestAppContext) {
    let (view, sink, vcx) = open(cx);
    set_text(
        &view,
        vcx,
        "select * from t where id = :id and n = :name",
        0,
    );
    vcx.simulate_keystrokes("ctrl-enter");
    vcx.run_until_parked();
    assert!(sink.requests.borrow().is_empty(), "waits for the prompt");
    assert_eq!(
        view.read_with(vcx, |v, _| v.prompt_labels()),
        [":id", ":name"]
    );
    view.update_in(vcx, |v, window, cx| {
        v.set_prompt_value(0, "42", window, cx);
        v.toggle_prompt_null(1, cx);
        v.confirm_prompt(window, cx);
    });
    vcx.run_until_parked();
    assert!(!view.read_with(vcx, |v, _| v.is_prompting()));
    let requests = sink.requests.borrow();
    assert_eq!(
        requests[0].items[0].text,
        "select * from t where id = ? and n = ?"
    );
    assert_eq!(
        requests[0].items[0].params,
        [Param::Text("42".into()), Param::Null]
    );
    assert!(!format!("{:?}", requests[0]).contains("42"));
}

#[gpui::test]
fn cancel_closes_the_prompt_or_cancels_the_run(cx: &mut TestAppContext) {
    let (view, sink, vcx) = open(cx);
    set_text(&view, vcx, "select :a", 0);
    vcx.simulate_keystrokes("ctrl-enter");
    vcx.run_until_parked();
    assert!(view.read_with(vcx, |v, _| v.is_prompting()));
    view.update_in(vcx, |v, window, cx| v.cancel_prompt(window, cx));
    assert!(!view.read_with(vcx, |v, _| v.is_prompting()));
    assert!(sink.requests.borrow().is_empty());

    set_text(&view, vcx, "select 1", 0);
    vcx.simulate_keystrokes("ctrl-enter");
    vcx.simulate_keystrokes("ctrl-.");
    vcx.run_until_parked();
    let id = sink.requests.borrow()[0].id;
    assert_eq!(*sink.cancelled.borrow(), [id]);
    view.update(vcx, |v, cx| v.finish_run(id, cx));
    assert_eq!(view.read_with(vcx, |v, _| v.running()), None);
}

#[gpui::test]
fn error_positions_are_shown_and_reachable(cx: &mut TestAppContext) {
    let (view, sink, vcx) = open(cx);
    set_text(&view, vcx, "select 1;\nselect * frm t", 12);
    vcx.simulate_keystrokes("ctrl-enter");
    vcx.run_until_parked();
    let id = sink.requests.borrow()[0].id;
    let error = StatementError {
        request: id,
        item: 0,
        message: "syntax error at or near \"frm\"".into(),
        code: Some("42601".into()),
        location: Some(ErrorLocation::CharOffset(10)),
    };
    // An answer to another request is ignored.
    view.update(vcx, |v, cx| {
        let stale = StatementError {
            request: RequestId(99),
            ..error.clone()
        };
        v.report_error(&stale, cx);
    });
    assert!(view.read_with(vcx, |v, _| v.error().is_none()));
    view.update(vcx, |v, cx| v.report_error(&error, cx));
    let placed = view.read_with(vcx, |v, _| v.error().and_then(|e| e.placed.clone()));
    let Some(placed) = placed else {
        panic!("placed")
    };
    assert_eq!((placed.position.line, placed.position.column), (1, 9));
    assert_eq!(diagnostics(&view, vcx), 1);
    view.update_in(vcx, |v, _, cx| {
        v.editor()
            .update(cx, |s, cx| s.set_selected_range(0..0, cx));
    });
    vcx.simulate_keystrokes("f8");
    vcx.run_until_parked();
    assert_eq!(selected(&view, vcx), placed.range.start..placed.range.start);
}

#[gpui::test]
fn ime_commits_lose_control_characters(cx: &mut TestAppContext) {
    let (view, _sink, vcx) = open(cx);
    set_text(&view, vcx, "", 0);
    view.update_in(vcx, |v, window, cx| {
        v.editor().update(cx, |s, cx| {
            s.replace_text_in_range(None, "sel\u{7}ect\r\n1\u{1b}", window, cx);
        });
    });
    vcx.run_until_parked();
    assert_eq!(text(&view, vcx), "select\n1");
    assert_eq!(view.read_with(vcx, |v, _| v.removed_controls()), 3);
    let end = "select\n1".len();
    assert_eq!(
        selected(&view, vcx),
        end..end,
        "cursor stays after the commit"
    );
}

#[gpui::test]
fn pastes_lose_control_characters(cx: &mut TestAppContext) {
    let (view, _sink, vcx) = open(cx);
    set_text(&view, vcx, "x", 1);
    vcx.write_to_clipboard(ClipboardItem::new_string("a\u{0}b\u{9b}c".into()));
    vcx.dispatch_action(Paste);
    vcx.run_until_parked();
    assert_eq!(text(&view, vcx), "xabc");
    // A clean clipboard goes through gpui-component's own paste.
    vcx.write_to_clipboard(ClipboardItem::new_string(" d".into()));
    vcx.dispatch_action(Paste);
    vcx.run_until_parked();
    assert_eq!(text(&view, vcx), "xabc d");
}

#[gpui::test]
fn bidi_controls_are_flagged_and_selectable(cx: &mut TestAppContext) {
    let (view, sink, vcx) = open(cx);
    set_text(&view, vcx, "select 1", 8);
    view.update_in(vcx, |v, window, cx| {
        v.editor().update(cx, |s, cx| {
            s.replace_text_in_range(None, " -- \u{202e}x\u{2066}", window, cx);
        });
    });
    vcx.run_until_parked();
    let labels: Vec<String> = view.read_with(vcx, |v, _| {
        v.bidi_controls().iter().map(|b| b.label()).collect()
    });
    assert_eq!(
        labels,
        [
            "U+202E RIGHT-TO-LEFT OVERRIDE",
            "U+2066 LEFT-TO-RIGHT ISOLATE"
        ]
    );
    assert_eq!(
        diagnostics(&view, vcx),
        2,
        "one underlined warning per control"
    );
    vcx.simulate_keystrokes("alt-f8");
    vcx.run_until_parked();
    assert_eq!(selected(&view, vcx).len(), '\u{2066}'.len_utf8());
    // Runs carry the count, for the confirmation dialogs.
    vcx.simulate_keystrokes("alt-x");
    vcx.run_until_parked();
    assert_eq!(sink.requests.borrow()[0].bidi_controls, 2);
}

#[gpui::test]
fn comment_toggle_and_next_occurrence(cx: &mut TestAppContext) {
    let (view, _sink, vcx) = open(cx);
    set_text(&view, vcx, "select id\nfrom t where id = 1", 3);
    vcx.simulate_keystrokes("ctrl-/");
    vcx.run_until_parked();
    assert_eq!(text(&view, vcx), "-- select id\nfrom t where id = 1");
    vcx.simulate_keystrokes("ctrl-/");
    vcx.run_until_parked();
    assert_eq!(text(&view, vcx), "select id\nfrom t where id = 1");

    view.update_in(vcx, |v, _, cx| {
        v.editor()
            .update(cx, |s, cx| s.set_selected_range(8..8, cx));
    });
    vcx.simulate_keystrokes("ctrl-d");
    assert_eq!(selected(&view, vcx), 7..9);
    vcx.simulate_keystrokes("ctrl-d");
    vcx.simulate_keystrokes("ctrl-d");
    // Both occurrences are selected: typing replaces them together.
    vcx.simulate_input("x");
    vcx.run_until_parked();
    assert_eq!(text(&view, vcx), "select x\nfrom t where x = 1");
}

#[gpui::test]
fn without_a_sink_nothing_runs(cx: &mut TestAppContext) {
    init(cx);
    let (view, vcx) = cx.add_window_view(|window, cx| SqlEditor::new(setup(), window, cx));
    view.update_in(vcx, |v, window, cx| {
        v.set_text("select 1", window, cx);
        v.execute(ExecutionKind::Script, window, cx);
    });
    let notice = view.read_with(vcx, |v, _| v.notice().map(ToString::to_string));
    assert_eq!(
        notice.as_deref(),
        Some("No connection: choose one for this editor first")
    );
}

#[gpui::test]
fn loaded_scripts_are_sanitized(cx: &mut TestAppContext) {
    let (view, _sink, vcx) = open(cx);
    let removed = view.update_in(vcx, |v, window, cx| v.set_text("a\r\nb\u{0}", window, cx));
    assert_eq!(removed, 2);
    assert_eq!(text(&view, vcx), "a\nb");
}

#[gpui::test]
fn transform_formats_the_selection_or_the_document(cx: &mut TestAppContext) {
    let (view, _, vcx) = open(cx);
    set_text(&view, vcx, "select 1; select 2", 0);
    let changed = view.update_in(vcx, |v, window, cx| {
        v.transform_selection_or_all(str::to_uppercase, window, cx)
    });
    assert!(changed);
    assert_eq!(text(&view, vcx), "SELECT 1; SELECT 2");
    // Only the selection when there is one; the output is sanitized.
    view.update_in(vcx, |v, _, cx| {
        v.editor()
            .update(cx, |s, cx| s.set_selected_range(0..8, cx));
    });
    view.update_in(vcx, |v, window, cx| {
        v.transform_selection_or_all(|_| "select\u{0}1".to_owned(), window, cx)
    });
    assert_eq!(text(&view, vcx), "select1; SELECT 2");
    let unchanged = view.update_in(vcx, |v, window, cx| {
        v.transform_selection_or_all(str::to_owned, window, cx)
    });
    assert!(!unchanged);
}

#[gpui::test]
fn insert_text_places_the_caret(cx: &mut TestAppContext) {
    let (view, _, vcx) = open(cx);
    set_text(&view, vcx, "-- x\n", 5);
    view.update_in(vcx, |v, window, cx| {
        v.insert_text("SELECT * FROM t WHERE ", Some(7), window, cx);
    });
    assert_eq!(text(&view, vcx), "-- x\nSELECT * FROM t WHERE ");
    assert_eq!(selected(&view, vcx), 12..12);
}

#[gpui::test]
fn built_statements_are_shown_and_sent_with_their_values(cx: &mut TestAppContext) {
    let (view, sink, vcx) = open(cx);
    let text = "SELECT * FROM \"t\" WHERE \"id\" = $1";
    let id = view.update_in(vcx, |v, window, cx| {
        v.submit_built(
            text,
            vec![ninesql_core::driver_api::Param::Int(7)],
            window,
            cx,
        )
    });
    assert!(id.is_ok());
    assert_eq!(text_of(&view, vcx), text);
    {
        let requests = sink.requests.borrow();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].items[0].text, text);
        assert_eq!(requests[0].items[0].params.len(), 1);
    }
    // Refused while it runs, and for text the editor would change.
    let again = view.update_in(vcx, |v, window, cx| {
        v.submit_built(text, Vec::new(), window, cx)
    });
    assert!(again.is_err());
    view.update_in(vcx, |v, _, cx| {
        if let Ok(id) = id {
            v.finish_run(id, cx);
        }
    });
    let hidden = view.update_in(vcx, |v, window, cx| {
        v.submit_built("SELECT 1 WHERE a = '\u{7}x'", Vec::new(), window, cx)
    });
    assert!(hidden.is_err());
    assert_eq!(sink.requests.borrow().len(), 1);
}

fn text_of(view: &Entity<SqlEditor>, vcx: &mut VisualTestContext) -> String {
    text(view, vcx)
}
