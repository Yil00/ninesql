//! The SQL editor view: gpui-component's code editor (`EditorState`, tree-sitter SQL
//! highlighting, search / replace, multi-cursor, folding, completion menu, diagnostics) wrapped
//! with NineSQL's commit-path hygiene, execution requests, parameters prompt, error positions
//! and bidirectional-control warnings.

use std::ops::Range;
use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::highlighter::{Diagnostic, DiagnosticSeverity};
use gpui_kit::component::input::{
    Editor, EditorState, Input, InputEditsEvent, InputState, Paste, Replace, RopeExt as _, Search,
    ShowCompletions,
};
use gpui_kit::component::{ActiveTheme as _, Selectable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use ninesql_core::ConnectionId;

use crate::actions::{
    CancelExecution, EDITOR_CONTEXT, ExecuteScript, ExecuteSelection, ExecuteStatement, Find,
    FindReplace, GoToError, SelectBidiControls, SelectNextOccurrence, ToggleComment,
    TriggerCompletion,
};
use crate::completion::{CompletionEngine, MetadataProvider, NoMetadata, SqlCompletionProvider};
use crate::edit_ops::{next_occurrence, toggle_line_comment, word_at};
use crate::execution::{
    ExecutionKind, ExecutionPlan, ExecutionRequest, ExecutionSink, PlacedError, RequestId,
    StatementError, place_error,
};
use crate::hygiene::{self, BidiControl};
use crate::params::{ParameterPrompt, ParameterStyle};
use crate::script::ScriptPath;
use crate::split::StatementSplitter;

/// What the editor needs from its connection: the dialect's splitter, parameter style and
/// completion settings, and the metadata cache.
#[derive(Clone)]
pub struct EditorSetup {
    /// The dialect's splitting.
    pub splitter: Rc<dyn StatementSplitter>,
    /// Placeholder syntax of the dialect.
    pub parameter_style: ParameterStyle,
    /// Keywords and quoting for completion.
    pub completion: CompletionEngine,
    /// The core's metadata cache for the connection.
    pub metadata: Rc<dyn MetadataProvider>,
}

impl EditorSetup {
    /// Setup over a splitter, with ANSI parameters, keywords and no metadata.
    #[must_use]
    pub fn new(splitter: Rc<dyn StatementSplitter>) -> Self {
        Self {
            splitter,
            parameter_style: ParameterStyle::ANSI,
            completion: CompletionEngine::new(crate::completion::KeywordSet::Ansi),
            metadata: Rc::new(NoMetadata),
        }
    }
}

/// An error shown in the editor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShownError {
    /// Display-only engine message.
    pub message: String,
    /// Where it is in the document, when known and the statement was not edited since.
    pub placed: Option<PlacedError>,
}

struct PendingRun {
    plan: ExecutionPlan,
    prompt: ParameterPrompt,
    inputs: Vec<Entity<InputState>>,
    nulls: Vec<bool>,
}

/// Emitted by [`SqlEditor`] when its unsaved state flips ([`SqlEditor::is_dirty`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DirtyChanged(pub bool);

/// A fingerprint of a text (length and hash), to tell an edited buffer from its saved copy
/// without keeping a second copy of the text.
fn fingerprint(text: &str) -> (usize, u64) {
    use std::hash::Hasher as _;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    hasher.write(text.as_bytes());
    (text.len(), hasher.finish())
}

/// The SQL editor view.
pub struct SqlEditor {
    editor: Entity<EditorState>,
    setup: EditorSetup,
    sink: Option<Rc<dyn ExecutionSink>>,
    connection: Option<ConnectionId>,
    script: Option<ScriptPath>,
    next_request: u64,
    /// The last request, **without its parameter values** (I1: not kept after execution).
    last_request: Option<ExecutionRequest>,
    running: Option<RequestId>,
    pending: Option<PendingRun>,
    error: Option<ShownError>,
    notice: Option<SharedString>,
    bidi: Vec<BidiControl>,
    removed_controls: usize,
    occurrences: Vec<Range<usize>>,
    /// Fingerprint of the text as last saved or loaded; `None` for a buffer never saved.
    saved: Option<(usize, u64)>,
    dirty: bool,
    /// First line to scroll to once the editor is laid out (session restore).
    pending_scroll: Option<usize>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<DirtyChanged> for SqlEditor {}

impl SqlEditor {
    /// An empty editor.
    pub fn new(setup: EditorSetup, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("sql")
                .line_number(true)
                .folding(true)
        });
        let provider = SqlCompletionProvider::new(
            setup.completion.clone(),
            setup.splitter.clone(),
            setup.metadata.clone(),
        );
        editor.update(cx, |state, _| {
            state.lsp_mut().completion_provider = Some(Rc::new(provider));
        });
        let subscriptions = vec![cx.subscribe_in(&editor, window, Self::on_edits)];
        Self {
            editor,
            setup,
            sink: None,
            connection: None,
            script: None,
            next_request: 1,
            last_request: None,
            running: None,
            pending: None,
            error: None,
            notice: None,
            bidi: Vec::new(),
            removed_controls: 0,
            occurrences: Vec::new(),
            saved: None,
            dirty: false,
            pending_scroll: None,
            _subscriptions: subscriptions,
        }
    }

    // --- script files ------------------------------------------------------------------------

    /// Loads a script file's text (read by the storage layer): sanitized like typed text
    /// ([`Self::set_text`]), the caret at the start, the buffer marked as saved with `raw` as
    /// its saved copy. When sanitizing changed the text, the buffer differs from the file and
    /// shows as unsaved. Returns the number of characters removed or changed.
    pub fn load_script(
        &mut self,
        raw: &str,
        script: ScriptPath,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        let fixes = self.set_text(raw, window, cx);
        self.editor
            .update(cx, |s, cx| s.set_selected_range(0..0, cx));
        self.script = Some(script);
        self.saved = Some(fingerprint(raw));
        if fixes > 0 {
            self.notice = Some(SharedString::from(format!(
                "{fixes} control character(s) or line end(s) changed on load: saving writes the cleaned text"
            )));
        }
        self.refresh_dirty(cx);
        fixes
    }

    /// The buffer was written to `script` with the text `saved`.
    pub fn mark_saved(&mut self, script: ScriptPath, saved: &str, cx: &mut Context<Self>) {
        self.script = Some(script);
        self.saved = Some(fingerprint(saved));
        self.refresh_dirty(cx);
    }

    /// Whether the buffer holds text that is not saved: edited since its script was loaded or
    /// saved, or, for a buffer never saved, any text at all (which would be lost on close and
    /// is never restored, I1).
    #[must_use]
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    fn refresh_dirty(&mut self, cx: &mut Context<Self>) {
        let len = self.editor.read(cx).text().len();
        let dirty = match self.saved {
            None => len > 0,
            Some((saved_len, _)) if saved_len != len => true,
            Some(saved) => fingerprint(&self.text(cx)) != saved,
        };
        if dirty != self.dirty {
            self.dirty = dirty;
            cx.emit(DirtyChanged(dirty));
            cx.notify();
        }
    }

    /// The caret as a 0-based line and column (in `char`s).
    #[must_use]
    pub fn cursor_line_column(&self, cx: &App) -> (usize, usize) {
        let text = self.text(cx);
        let cursor = self.editor.read(cx).cursor();
        let position = crate::text::LineIndex::new(&text).position(cursor);
        (position.line, position.column)
    }

    /// Puts the caret at a 0-based line and column (in `char`s), clamped to the text.
    pub fn set_cursor_line_column(&mut self, line: usize, column: usize, cx: &mut Context<Self>) {
        let text = self.text(cx);
        let offset = crate::text::LineIndex::new(&text).offset(line, column);
        self.editor
            .update(cx, |s, cx| s.set_selected_range(offset..offset, cx));
    }

    /// The first visible line (0 before the editor is laid out).
    #[must_use]
    pub fn scroll_line(&self, cx: &App) -> usize {
        let state = self.editor.read(cx);
        let Some(height) = state.line_height().filter(|h| *h > px(0.)) else {
            return self.pending_scroll.unwrap_or(0);
        };
        let top = -f32::from(state.scroll_offset().y);
        if top <= 0. {
            return 0;
        }
        (top / f32::from(height)).floor() as usize
    }

    /// Scrolls so that `line` is the first visible line, once the editor is laid out.
    pub fn set_scroll_line(&mut self, line: usize, cx: &mut Context<Self>) {
        self.pending_scroll = (line > 0).then_some(line);
        cx.notify();
    }

    fn apply_pending_scroll(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(line) = self.pending_scroll else {
            return;
        };
        match self.editor.read(cx).line_height() {
            Some(height) => {
                self.pending_scroll = None;
                // A restored position past the end (the file is shorter, or the session file
                // was edited) is clamped to the last line.
                let last = crate::text::LineIndex::new(&self.text(cx))
                    .line_count()
                    .saturating_sub(1);
                let line = line.min(last);
                let y = -(f32::from(height) * line as f32);
                self.editor.update(cx, |s, cx| {
                    s.set_scroll_offset(point(px(0.), px(y)), cx);
                });
            }
            // Not laid out yet: try again on the next frame.
            None => cx.on_next_frame(window, |_, _, cx| cx.notify()),
        }
    }

    /// The underlying gpui-component editor state.
    #[must_use]
    pub fn editor(&self) -> &Entity<EditorState> {
        &self.editor
    }

    /// The document text.
    #[must_use]
    pub fn text(&self, cx: &App) -> String {
        self.editor.read(cx).value().to_string()
    }

    /// Replaces the document (a script just read by the storage layer), sanitized like typed
    /// text. Returns the number of characters removed or changed.
    pub fn set_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) -> usize {
        let fixes = hygiene::fixes(text);
        let clean = hygiene::apply_fixes(text, &fixes);
        self.editor
            .update(cx, |s, cx| s.set_value(clean.clone(), window, cx));
        self.removed_controls = fixes.len();
        self.error = None;
        self.last_request = None;
        self.refresh_bidi(&clean, cx);
        self.refresh_dirty(cx);
        cx.notify();
        fixes.len()
    }

    /// Replaces the selection with `f(selected text)`, or the whole document with `f(text)`
    /// when nothing is selected ("Format SQL"). The result is sanitized like typed text and
    /// goes through the input's undo history. Returns `false` when `f` changed nothing.
    pub fn transform_selection_or_all(
        &mut self,
        f: impl FnOnce(&str) -> String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let (text, selection) = {
            let s = self.editor.read(cx);
            (s.value().to_string(), s.selected_range())
        };
        let range = if selection.is_empty() {
            0..text.len()
        } else {
            selection
        };
        let Some(source) = text.get(range.clone()) else {
            return false;
        };
        let out = f(source);
        if out == source {
            return false;
        }
        let clean = hygiene::apply_fixes(&out, &hygiene::fixes(&out));
        let end = range.start + clean.len();
        self.editor.update(cx, |s, cx| {
            s.set_selected_range(range.clone(), cx);
            s.replace(clean.clone(), window, cx);
            s.set_selected_range(range.start..end, cx);
        });
        let full = self.text(cx);
        self.refresh_bidi(&full, cx);
        cx.notify();
        true
    }

    /// Inserts `text` in place of the selection (a snippet expansion), sanitized like typed
    /// text, and puts the caret at byte `cursor` of the inserted text (its end without one).
    pub fn insert_text(
        &mut self,
        text: &str,
        cursor: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let fixes = hygiene::fixes(text);
        let clean = hygiene::apply_fixes(text, &fixes);
        // A sanitized text may be shorter: keep the caret inside it.
        let caret = cursor
            .filter(|_| fixes.is_empty())
            .unwrap_or(clean.len())
            .min(clean.len());
        let start = self.editor.read(cx).selected_range().start;
        self.editor.update(cx, |s, cx| {
            s.replace(clean.clone(), window, cx);
            s.set_selected_range(start + caret..start + caret, cx);
        });
        let full = self.text(cx);
        self.refresh_bidi(&full, cx);
        cx.notify();
    }

    /// Replaces the setup (the editor's connection changed): splitting, parameter style,
    /// completion keywords, quoting and metadata follow the new dialect. The text is kept.
    pub fn set_setup(&mut self, setup: EditorSetup, cx: &mut Context<Self>) {
        let provider = SqlCompletionProvider::new(
            setup.completion.clone(),
            setup.splitter.clone(),
            setup.metadata.clone(),
        );
        self.editor.update(cx, |state, _| {
            state.lsp_mut().completion_provider = Some(Rc::new(provider));
        });
        self.setup = setup;
        cx.notify();
    }

    /// The current setup.
    #[must_use]
    pub fn setup(&self) -> &EditorSetup {
        &self.setup
    }

    /// Shows an info message under the editor ("no connection"…).
    pub fn set_notice(&mut self, notice: Option<SharedString>, cx: &mut Context<Self>) {
        self.notice = notice;
        cx.notify();
    }

    /// Sends execution requests to `sink` (the core's execution service).
    pub fn set_sink(&mut self, sink: Rc<dyn ExecutionSink>) {
        self.sink = Some(sink);
    }

    /// The connection runs go to.
    pub fn set_connection(&mut self, connection: Option<ConnectionId>, cx: &mut Context<Self>) {
        self.connection = connection;
        cx.notify();
    }

    /// The editor's connection.
    #[must_use]
    pub fn connection(&self) -> Option<ConnectionId> {
        self.connection
    }

    /// The script file shown, if saved.
    pub fn set_script(&mut self, script: Option<ScriptPath>, cx: &mut Context<Self>) {
        self.script = script;
        cx.notify();
    }

    /// The script file shown.
    #[must_use]
    pub fn script(&self) -> Option<&ScriptPath> {
        self.script.as_ref()
    }

    /// The current error, if any.
    #[must_use]
    pub fn error(&self) -> Option<&ShownError> {
        self.error.as_ref()
    }

    /// The last info message ("nothing to run here"…).
    #[must_use]
    pub fn notice(&self) -> Option<&SharedString> {
        self.notice.as_ref()
    }

    /// Bidirectional control characters in the document.
    #[must_use]
    pub fn bidi_controls(&self) -> &[BidiControl] {
        &self.bidi
    }

    /// Control characters removed from typed, committed or pasted text since the editor opened.
    #[must_use]
    pub fn removed_controls(&self) -> usize {
        self.removed_controls
    }

    /// Whether the parameters prompt is open.
    #[must_use]
    pub fn is_prompting(&self) -> bool {
        self.pending.is_some()
    }

    /// Labels of the open prompt's entries.
    #[must_use]
    pub fn prompt_labels(&self) -> Vec<String> {
        self.pending.as_ref().map_or_else(Vec::new, |p| {
            p.prompt.entries().iter().map(|e| e.label()).collect()
        })
    }

    /// The request running, if any.
    #[must_use]
    pub fn running(&self) -> Option<RequestId> {
        self.running
    }

    // --- commit-path hygiene -----------------------------------------------------------------

    fn on_edits(
        &mut self,
        state: &Entity<EditorState>,
        event: &InputEditsEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut dirty = false;
        let mut bidi_touched = !self.bidi.is_empty();
        for (edit, rope) in event.edits() {
            let end = edit.new_end_byte.min(rope.len());
            let start = edit.start_byte.min(end);
            let inserted = rope.slice(start..end).to_string();
            dirty |= hygiene::needs_sanitizing(&inserted);
            bidi_touched |= !hygiene::find_bidi_controls(&inserted).is_empty();
        }
        // Never rewrite text under an open IME composition: the commit comes back here.
        let composing = state.update(cx, |s, cx| {
            use gpui_kit::EntityInputHandler as _;
            s.marked_text_range(window, cx).is_some()
        });
        if dirty && !composing {
            self.scrub(window, cx);
        }
        if bidi_touched {
            let text = self.text(cx);
            self.refresh_bidi(&text, cx);
        }
        self.occurrences.clear();
        self.refresh_dirty(cx);
    }

    /// Repairs the document in place: the minimal edits of [`hygiene::fixes`], cursor kept.
    fn scrub(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.text(cx);
        let fixes = hygiene::fixes(&text);
        if fixes.is_empty() {
            return;
        }
        self.removed_controls += fixes.len();
        let cursor = self.editor.read(cx).cursor();
        let shift: isize = fixes
            .iter()
            .filter(|(r, _)| r.end <= cursor)
            .map(|(r, rep)| rep.len() as isize - r.len() as isize)
            .sum();
        let new_cursor = cursor.saturating_add_signed(shift);
        self.editor.update(cx, |s, cx| {
            for (range, rep) in fixes.iter().rev() {
                s.set_selected_range(range.clone(), cx);
                s.replace(*rep, window, cx);
            }
            s.set_selected_range(new_cursor..new_cursor, cx);
        });
        self.notice = Some(SharedString::from(format!(
            "{} control character(s) removed from the inserted text",
            fixes.len()
        )));
        cx.notify();
    }

    /// Paste: sanitized before it reaches the editor when the clipboard holds control
    /// characters (otherwise gpui-component's own paste runs, with its multi-cursor handling).
    fn on_paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|c| c.text()) else {
            return;
        };
        if !hygiene::needs_sanitizing(&text) {
            return;
        }
        cx.stop_propagation();
        let fixes = hygiene::fixes(&text);
        let clean = hygiene::apply_fixes(&text, &fixes);
        self.removed_controls += fixes.len();
        self.editor
            .update(cx, |s, cx| s.replace(clean.clone(), window, cx));
        self.notice = Some(SharedString::from(format!(
            "{} control character(s) removed from the pasted text",
            fixes.len()
        )));
        cx.notify();
    }

    fn refresh_bidi(&mut self, text: &str, cx: &mut Context<Self>) {
        self.bidi = hygiene::find_bidi_controls(text);
        self.refresh_diagnostics(cx);
    }

    /// Rebuilds the editor's diagnostics: the error (if placed) and one warning per
    /// bidirectional control, so each is underlined and listed in the gutter.
    fn refresh_diagnostics(&mut self, cx: &mut Context<Self>) {
        let text = self.text(cx);
        let error = self.error.clone();
        let bidi = self.bidi.clone();
        self.editor.update(cx, |s, cx| {
            let rope = s.text().clone();
            let Some(set) = s.diagnostics_mut() else {
                return;
            };
            set.reset(&rope);
            if let Some(ShownError {
                message,
                placed: Some(placed),
            }) = &error
            {
                let range = if placed.range.is_empty() {
                    placed.range.start..placed.range.start
                } else {
                    placed.range.clone()
                };
                set.push(
                    Diagnostic::new(
                        rope.offset_to_position(range.start)..rope.offset_to_position(range.end),
                        message.clone(),
                    )
                    .with_severity(DiagnosticSeverity::Error),
                );
            }
            for b in &bidi {
                let range = b.highlight_range(&text);
                set.push(
                    Diagnostic::new(
                        rope.offset_to_position(range.start)..rope.offset_to_position(range.end),
                        format!(
                            "Invisible bidirectional control {}: the text may run differently from how it reads",
                            b.label()
                        ),
                    )
                    .with_severity(DiagnosticSeverity::Warning),
                );
            }
            cx.notify();
        });
    }

    // --- execution ---------------------------------------------------------------------------

    /// Plans and submits a run of `kind`; opens the parameters prompt when a statement has
    /// placeholders.
    pub fn execute(&mut self, kind: ExecutionKind, window: &mut Window, cx: &mut Context<Self>) {
        if self.pending.is_some() {
            return;
        }
        let (text, selection, cursor) = {
            let s = self.editor.read(cx);
            (s.value().to_string(), s.selected_range(), s.cursor())
        };
        let plan = match ExecutionPlan::build(
            &text,
            selection,
            cursor,
            kind,
            self.setup.splitter.as_ref(),
            self.setup.parameter_style,
        ) {
            Ok(plan) => plan,
            Err(e) => {
                self.notice = Some(SharedString::from(e.to_string()));
                cx.notify();
                return;
            }
        };
        self.notice = None;
        if !plan.needs_parameters() {
            self.submit(plan, None, cx);
            return;
        }
        match ParameterPrompt::new(plan.parameter_refs()) {
            Ok(Some(prompt)) => {
                let inputs: Vec<Entity<InputState>> = prompt
                    .entries()
                    .iter()
                    .map(|_| cx.new(|cx| InputState::new(window, cx).placeholder("value")))
                    .collect();
                if let Some(first) = inputs.first() {
                    first.update(cx, |s, cx| s.focus(window, cx));
                }
                let nulls = vec![false; inputs.len()];
                self.pending = Some(PendingRun {
                    plan,
                    prompt,
                    inputs,
                    nulls,
                });
            }
            Ok(None) => self.submit(plan, None, cx),
            Err(e) => self.notice = Some(SharedString::from(e.to_string())),
        }
        cx.notify();
    }

    /// Sets prompt entry `index` to `value` (tests and the prompt's inputs).
    pub fn set_prompt_value(
        &mut self,
        index: usize,
        value: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(input) = self.pending.as_ref().and_then(|p| p.inputs.get(index)) {
            input.update(cx, |s, cx| s.set_value(value.to_owned(), window, cx));
        }
        if let Some(p) = self.pending.as_mut()
            && let Some(n) = p.nulls.get_mut(index)
        {
            *n = false;
        }
    }

    /// Toggles NULL for prompt entry `index`.
    pub fn toggle_prompt_null(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(p) = self.pending.as_mut()
            && let Some(n) = p.nulls.get_mut(index)
        {
            *n = !*n;
        }
        cx.notify();
    }

    /// Runs with the prompted values; the prompt and its inputs are dropped either way.
    pub fn confirm_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(mut pending) = self.pending.take() else {
            return;
        };
        for (i, input) in pending.inputs.iter().enumerate() {
            if pending.nulls.get(i).copied().unwrap_or(false) {
                pending.prompt.set_null(i);
            } else {
                let value = input.read(cx).value();
                pending.prompt.set_text(i, &value);
            }
        }
        // Drop the typed values from the inputs before they go.
        for input in &pending.inputs {
            input.update(cx, |s, cx| s.set_value("", window, cx));
        }
        let texts: Vec<String> = pending
            .plan
            .texts()
            .into_iter()
            .map(str::to_owned)
            .collect();
        let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        match pending.prompt.submit(&refs, self.setup.parameter_style) {
            Ok(bound) => self.submit(pending.plan, Some(bound), cx),
            Err(e) => self.notice = Some(SharedString::from(e.to_string())),
        }
        self.editor.update(cx, |s, cx| s.focus(window, cx));
        cx.notify();
    }

    /// Closes the prompt without running.
    pub fn cancel_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(pending) = self.pending.take() {
            for input in &pending.inputs {
                input.update(cx, |s, cx| s.set_value("", window, cx));
            }
        }
        self.editor.update(cx, |s, cx| s.focus(window, cx));
        cx.notify();
    }

    fn submit(
        &mut self,
        plan: ExecutionPlan,
        bound: Option<Vec<crate::params::BoundStatement>>,
        cx: &mut Context<Self>,
    ) {
        let Some(sink) = self.sink.clone() else {
            self.notice = Some("No connection: choose one for this editor first".into());
            return;
        };
        let id = RequestId(self.next_request);
        self.next_request += 1;
        let request = plan.into_request(id, self.connection, bound, self.bidi.len());
        // Keep the mapping for error positions, not the values (I1).
        let mut kept = request.clone();
        for item in &mut kept.items {
            item.params.clear();
        }
        self.last_request = Some(kept);
        self.running = Some(id);
        self.error = None;
        self.refresh_diagnostics(cx);
        sink.submit(request);
    }

    /// Replaces the document with `text` and submits it as one statement with `params` (a
    /// query the application built, such as the core's filtered table query, P5-A): the user
    /// sees exactly what runs. Refused (`Err` with the reason) while a run is going on, without
    /// a sink, or when the text holds characters the editor would strip or change (it would no
    /// longer be what runs). Returns the request id.
    ///
    /// # Errors
    /// The reason, shown by the caller.
    pub fn submit_built(
        &mut self,
        text: &str,
        params: Vec<ninesql_core::driver_api::Param>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<RequestId, String> {
        if self.running.is_some() {
            return Err("A run is already going on in this editor: cancel it first.".to_owned());
        }
        let Some(sink) = self.sink.clone() else {
            return Err("No connection: choose one for this editor first".to_owned());
        };
        if !hygiene::fixes(text).is_empty() {
            return Err(
                "The query holds control or invisible characters the editor would change: remove them from the filter."
                    .to_owned(),
            );
        }
        self.set_text(text, window, cx);
        let id = RequestId(self.next_request);
        self.next_request += 1;
        let item = crate::execution::ExecutionItem::built_statement(text.to_owned(), params);
        let mut kept_item = item.clone();
        kept_item.params.clear();
        let request = ExecutionRequest {
            id,
            kind: ExecutionKind::Statement,
            connection: self.connection,
            items: vec![item],
            range_text: text.to_owned(),
            bidi_controls: self.bidi.len(),
        };
        // Keep the mapping for error positions, not the values (I1).
        self.last_request = Some(ExecutionRequest {
            items: vec![kept_item],
            range_text: request.range_text.clone(),
            ..request.clone()
        });
        self.running = Some(id);
        self.error = None;
        self.refresh_diagnostics(cx);
        sink.submit(request);
        cx.notify();
        Ok(id)
    }

    /// The core reports an error for an item of a run. Answers to an older run are ignored.
    pub fn report_error(&mut self, error: &StatementError, cx: &mut Context<Self>) {
        let Some(request) = self.last_request.as_ref().filter(|r| r.id == error.request) else {
            return;
        };
        let text = self.text(cx);
        let placed = place_error(&text, request, error);
        self.error = Some(ShownError {
            message: error.message.clone(),
            placed,
        });
        self.refresh_diagnostics(cx);
        cx.notify();
    }

    /// The core reports that run `id` ended (success, error or cancel).
    pub fn finish_run(&mut self, id: RequestId, cx: &mut Context<Self>) {
        if self.running == Some(id) {
            self.running = None;
            cx.notify();
        }
    }

    // --- actions -----------------------------------------------------------------------------

    fn on_execute_statement(
        &mut self,
        _: &ExecuteStatement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.execute(ExecutionKind::Statement, window, cx);
    }

    fn on_execute_selection(
        &mut self,
        _: &ExecuteSelection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.execute(ExecutionKind::Selection, window, cx);
    }

    fn on_execute_script(
        &mut self,
        _: &ExecuteScript,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.execute(ExecutionKind::Script, window, cx);
    }

    fn on_cancel(&mut self, _: &CancelExecution, window: &mut Window, cx: &mut Context<Self>) {
        if self.pending.is_some() {
            self.cancel_prompt(window, cx);
            return;
        }
        if let (Some(id), Some(sink)) = (self.running, self.sink.as_ref()) {
            sink.cancel(id);
        }
    }

    fn on_trigger_completion(
        &mut self,
        _: &TriggerCompletion,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let focus = self.editor.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
        window.dispatch_action(Box::new(ShowCompletions), cx);
    }

    fn on_find(&mut self, _: &Find, window: &mut Window, cx: &mut Context<Self>) {
        window.dispatch_action(Box::new(Search), cx);
    }

    fn on_find_replace(&mut self, _: &FindReplace, window: &mut Window, cx: &mut Context<Self>) {
        window.dispatch_action(Box::new(Replace), cx);
    }

    /// Toggles `--` comments on the selected lines.
    pub fn toggle_comment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (text, selection) = {
            let s = self.editor.read(cx);
            (s.value().to_string(), s.selected_range())
        };
        let Some(edit) = toggle_line_comment(&text, selection) else {
            return;
        };
        self.editor.update(cx, |s, cx| {
            s.set_selected_range(edit.range.clone(), cx);
            s.replace(edit.text.clone(), window, cx);
            s.set_selected_range(edit.selection.clone(), cx);
        });
    }

    fn on_toggle_comment(
        &mut self,
        _: &ToggleComment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_comment(window, cx);
    }

    /// Adds the next occurrence of the selection as another cursor (selects the word under the
    /// cursor first when nothing is selected).
    pub fn select_next_occurrence(&mut self, cx: &mut Context<Self>) {
        let (text, active) = {
            let s = self.editor.read(cx);
            (s.value().to_string(), s.selected_range())
        };
        if active.is_empty() {
            if let Some(word) = word_at(&text, active.start) {
                self.editor
                    .update(cx, |s, cx| s.set_selected_range(word.clone(), cx));
                self.occurrences = vec![word];
            }
            return;
        }
        // gpui-component keeps the first selection active while cursors are added.
        if !self.occurrences.contains(&active) {
            self.occurrences = vec![active.clone()];
        }
        let needle = text[self.occurrences[0].clone()].to_owned();
        let from = self.occurrences.last().map_or(active.end, |r| r.end);
        if let Some(next) = next_occurrence(&text, &needle, from, &self.occurrences) {
            self.editor
                .update(cx, |s, cx| s.add_selection(next.clone(), cx));
            self.occurrences.push(next);
        }
    }

    fn on_select_next_occurrence(
        &mut self,
        _: &SelectNextOccurrence,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_next_occurrence(cx);
    }

    /// Moves the cursor to the placed error.
    pub fn go_to_error(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(placed) = self.error.as_ref().and_then(|e| e.placed.clone()) else {
            return;
        };
        self.editor.update(cx, |s, cx| {
            s.set_selected_range(placed.range.start..placed.range.start, cx);
            s.focus(window, cx);
        });
    }

    fn on_go_to_error(&mut self, _: &GoToError, window: &mut Window, cx: &mut Context<Self>) {
        self.go_to_error(window, cx);
    }

    /// Selects every bidirectional control character (one cursor each).
    pub fn select_bidi_controls(&mut self, cx: &mut Context<Self>) {
        let ranges: Vec<Range<usize>> = self.bidi.iter().map(|b| b.range.clone()).collect();
        let Some((first, rest)) = ranges.split_first() else {
            self.notice = Some("No bidirectional control characters".into());
            cx.notify();
            return;
        };
        self.editor.update(cx, |s, cx| {
            s.set_selected_range(first.clone(), cx);
            for r in rest {
                s.add_selection(r.clone(), cx);
            }
        });
    }

    fn on_select_bidi(&mut self, _: &SelectBidiControls, _: &mut Window, cx: &mut Context<Self>) {
        self.select_bidi_controls(cx);
    }

    fn render_prompt(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let pending = self.pending.as_ref()?;
        let rows = pending
            .prompt
            .entries()
            .iter()
            .zip(&pending.inputs)
            .enumerate()
            .map(|(i, (entry, input))| {
                let null = pending.nulls.get(i).copied().unwrap_or(false);
                h_flex()
                    .gap_2()
                    .child(div().w(px(180.)).child(entry.label()))
                    .child(
                        div()
                            .flex_1()
                            .when(!null, |d| d.child(Input::new(input)))
                            .when(null, |d| d.child("NULL")),
                    )
                    .child(
                        Button::new(("param-null", i))
                            .small()
                            .outline()
                            .label("NULL")
                            .selected(null)
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.toggle_prompt_null(i, cx)),
                            ),
                    )
            });
        Some(
            v_flex()
                .id("sql-editor-parameters")
                .gap_2()
                .p_2()
                .border_b_1()
                .border_color(cx.theme().border)
                .child("Parameters (values are not saved)")
                .children(rows)
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            Button::new("param-run")
                                .primary()
                                .small()
                                .label("Run")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.confirm_prompt(window, cx)
                                })),
                        )
                        .child(
                            Button::new("param-cancel")
                                .small()
                                .label("Cancel")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.cancel_prompt(window, cx)
                                })),
                        ),
                )
                .into_any_element(),
        )
    }
}

impl Focusable for SqlEditor {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Render for SqlEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.apply_pending_scroll(window, cx);
        let theme = cx.theme().clone();
        let bidi = (!self.bidi.is_empty()).then(|| {
            let first = self.bidi.first().map(BidiControl::label).unwrap_or_default();
            div()
                .px_2()
                .py_1()
                .bg(theme.warning)
                .text_color(theme.warning_foreground)
                .child(format!(
                    "{} bidirectional control character(s) in this script (first: {first}). Alt+F8 selects them.",
                    self.bidi.len()
                ))
        });
        let error = self.error.as_ref().map(|e| {
            let at = e.placed.as_ref().map_or_else(String::new, |p| {
                format!(
                    " (line {}, column {})",
                    p.position.line + 1,
                    p.position.column + 1
                )
            });
            div()
                .px_2()
                .py_1()
                .bg(theme.danger)
                .text_color(theme.danger_foreground)
                .child(format!("{}{at}", e.message))
        });
        let notice = self.notice.clone().map(|n| {
            div()
                .px_2()
                .py_1()
                .text_color(theme.muted_foreground)
                .child(n)
        });
        let prompt = self.render_prompt(cx);
        v_flex()
            .key_context(EDITOR_CONTEXT)
            .size_full()
            .capture_action(cx.listener(Self::on_paste))
            .on_action(cx.listener(Self::on_execute_statement))
            .on_action(cx.listener(Self::on_execute_selection))
            .on_action(cx.listener(Self::on_execute_script))
            .on_action(cx.listener(Self::on_cancel))
            .on_action(cx.listener(Self::on_trigger_completion))
            .on_action(cx.listener(Self::on_find))
            .on_action(cx.listener(Self::on_find_replace))
            .on_action(cx.listener(Self::on_toggle_comment))
            .on_action(cx.listener(Self::on_select_next_occurrence))
            .on_action(cx.listener(Self::on_go_to_error))
            .on_action(cx.listener(Self::on_select_bidi))
            .children(bidi)
            .children(prompt)
            .child(
                Editor::new(&self.editor)
                    .bordered(false)
                    .flex_1()
                    .font_family(theme.mono_font_family.clone())
                    .text_size(theme.mono_font_size),
            )
            .children(error)
            .children(notice)
    }
}
