//! An SQL editor tab (P3): `ninesql_editor::SqlEditor` above the results grid
//! (`ninesql_grid::ResultsView`), on one open session chosen with the connection picker.
//!
//! - **Setup.** The editor follows the session's dialect: `Dialect::split_script` and the shared
//!   lexer for splitting and segments, `ParameterStyle::for_engine`, `KeywordSet::for_engine` and
//!   `Dialect::quote_identifier` for completion. Metadata: the navigator lists connections only
//!   and caches no schema yet, so completion gets `NoMetadata` (keywords, aliases and CTE names
//!   only) until a metadata cache exists.
//! - **Execution.** The editor's requests reach [`SqlEditorTab::start`] through [`run::TabSink`];
//!   the tab builds a `RunRequest` (paged for the grid, under the preferences' budget) and calls
//!   `ExecutionService::run`, which runs on the driver runtime. Each `RunEvent` is awaited on
//!   GPUI's **background** executor and applied on the foreground thread (I6): forwarded to the
//!   grid as an `ExecutionEvent`, errors placed in the editor (`report_error`), the end reported
//!   with `finish_run`. Back-pressure is kept: the next event is only awaited once the previous
//!   one was applied.
//! - **Confirmations (ADR-0007, I3).** `NeedsConfirmation` opens the statement confirmation
//!   with the exact statement, the connection in its type color and the editor's count of
//!   bidirectional controls; "Run" resumes with
//!   `resume_from(span.start).with_script_state(state).confirmed(token)`,
//!   "Cancel" stops the run there.
//! - **Transactions (F-SQL-07).** The indicator follows `CoreEvent::TransactionChanged`
//!   (through the hub) and the state reported after each statement; commit and roll back are
//!   offered only when the driver declares manual commit (I8).
//! - **Cancel (F-SQL-05).** `CancelExecution` (editor) and the toolbar's button cancel the run;
//!   the button says "Stop fetching" when the driver can only stop locally
//!   (`CancelSupport::LocalOnly`).

pub mod run;

use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::base::async_util::unbounded;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::resizable::{resizable_panel, v_resizable};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use ninesql_core::driver_api::{AnsiDialect, CancelSupport};
use ninesql_core::{
    ConfirmationRequest, ConnectionId, CoreError, RunEnd, RunEvent, RunRequest, SessionHandle,
    SessionId, TransactionState,
};
use ninesql_editor::completion::{CompletionEngine, KeywordSet, NoMetadata};
use ninesql_editor::execution::{ExecutionRequest, StatementError};
use ninesql_editor::params::ParameterStyle;
use ninesql_editor::split::DialectSplitter;
use ninesql_editor::{EditorSetup, SqlEditor};
use ninesql_grid::{ExecutionEvent, GridBudget, ResultsView};

use crate::connections::connect_flow::open_statement_confirmation;
use crate::connections::flow::StatementConfirmView;
use crate::connections::hub::{ConnectionHub, HubEvent};
use crate::connections::visuals::{ConnectionBadges, badges, hsla};
use run::{CurrentRun, RunSource, SharedRun, TabSink};

/// The editor setup for `session` (its dialect), or ANSI without a session.
#[must_use]
pub fn setup_for(session: Option<&SessionHandle>) -> EditorSetup {
    let Some(session) = session else {
        return EditorSetup::new(Rc::new(DialectSplitter::new(Arc::new(AnsiDialect))));
    };
    let dialect = session.dialect();
    let engine = &session.info().capabilities.engine;
    let quoting = dialect.clone();
    EditorSetup {
        splitter: Rc::new(DialectSplitter::new(dialect)),
        parameter_style: ParameterStyle::for_engine(engine),
        completion: CompletionEngine::new(KeywordSet::for_engine(engine))
            .with_quoter(Rc::new(move |ident: &str| quoting.quote_identifier(ident))),
        // The navigator caches no schema metadata yet (connections only).
        metadata: Rc::new(NoMetadata),
    }
}

/// A statement waiting for the user's confirmation.
#[derive(Clone)]
struct PendingConfirmation {
    index: usize,
    span: Range<usize>,
    request: ConfirmationRequest,
    /// Splitter state in force at the statement (MySQL `DELIMITER`), passed back on resume.
    script_state: ninesql_core::driver_api::ScriptState,
}

/// The run of one editor request, across confirmations.
struct ActiveRun {
    /// The editor's request (bind values included: kept only while the run lasts, for a resume
    /// after a confirmation; never logged).
    request: ExecutionRequest,
    /// The core request, to resume from.
    template: RunRequest,
    /// Grid number of statement 0 of the current core run.
    base: u32,
    /// Buffer span of each started statement of the current core run.
    spans: HashMap<usize, Range<usize>>,
    confirmation: Option<PendingConfirmation>,
    /// Bumped at each core run: events of an older one are ignored.
    generation: u64,
    task: Option<Task<()>>,
}

/// What the toolbar shows about the last action (errors of transaction commands, refusals).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TabMessage {
    /// Text.
    pub text: SharedString,
    /// An error (shown in the danger color) or an info.
    pub error: bool,
}

/// One editor tab.
pub struct SqlEditorTab {
    title: SharedString,
    editor: Entity<SqlEditor>,
    results: Entity<ResultsView>,
    hub: Entity<ConnectionHub>,
    session: Option<SessionHandle>,
    current: SharedRun,
    run: Option<ActiveRun>,
    generation: u64,
    transaction: Option<TransactionState>,
    message: Option<TabMessage>,
    budget: GridBudget,
    _tasks: Vec<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl SqlEditorTab {
    /// An empty SQL editor titled `title`, on `session` if any.
    pub fn new(
        title: impl Into<SharedString>,
        hub: Entity<ConnectionHub>,
        session: Option<SessionHandle>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let fetch = hub.read(cx).preferences().execution.budget();
        let budget = GridBudget::from_fetch_budget(&fetch);
        let current: SharedRun = Rc::new(RefCell::new(None));
        let (tx, rx) = unbounded::<ExecutionRequest>();
        let setup = setup_for(session.as_ref());
        let connection = session.as_ref().map(|s| s.info().connection);
        let sink = Rc::new(TabSink::new(tx, current.clone()));
        let editor = cx.new(|cx| {
            let mut editor = SqlEditor::new(setup, window, cx);
            editor.set_sink(sink);
            editor.set_connection(connection, cx);
            editor
        });
        let source = Rc::new(RunSource::new(current.clone()));
        let results = cx.new(|cx| {
            let mut view = ResultsView::new(budget, cx);
            view.set_source(source);
            view
        });
        let requests = cx.spawn_in(window, async move |this, cx| {
            while let Ok(request) = rx.recv().await {
                if this
                    .update_in(cx, |tab, window, cx| tab.start(request, window, cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        let subscriptions = vec![
            cx.subscribe(&hub, Self::on_hub_event),
            cx.observe(&hub, |_, _, cx| cx.notify()),
        ];
        let transaction = session.as_ref().map(SessionHandle::transaction_state);
        Self {
            title: title.into(),
            editor,
            results,
            hub,
            session,
            current,
            run: None,
            generation: 0,
            transaction,
            message: None,
            budget,
            _tasks: vec![requests],
            _subscriptions: subscriptions,
        }
    }

    /// Tab title.
    #[must_use]
    pub fn title(&self) -> &SharedString {
        &self.title
    }

    /// The SQL editor.
    #[must_use]
    pub fn editor(&self) -> &Entity<SqlEditor> {
        &self.editor
    }

    /// The results grid.
    #[must_use]
    pub fn results(&self) -> &Entity<ResultsView> {
        &self.results
    }

    /// The session runs go to (`None` once it closed).
    #[must_use]
    pub fn session(&self) -> Option<&SessionHandle> {
        self.session.as_ref().filter(|s| !s.is_closed())
    }

    /// The connection of the tab's session.
    #[must_use]
    pub fn connection(&self) -> Option<ConnectionId> {
        self.session().map(|s| s.info().connection)
    }

    /// The last transaction state known for the session.
    #[must_use]
    pub fn transaction(&self) -> Option<TransactionState> {
        self.session()?;
        self.transaction
    }

    /// The toolbar message.
    #[must_use]
    pub fn message(&self) -> Option<&TabMessage> {
        self.message.as_ref()
    }

    /// Whether a run is going on (including one waiting for a confirmation).
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.run.is_some()
    }

    /// Whether a statement waits for the user's confirmation.
    #[must_use]
    pub fn awaiting_confirmation(&self) -> bool {
        self.run.as_ref().is_some_and(|r| r.confirmation.is_some())
    }

    /// The statement waiting for confirmation, if any.
    #[must_use]
    pub fn pending_statement(&self) -> Option<&str> {
        self.run
            .as_ref()?
            .confirmation
            .as_ref()
            .map(|c| c.request.statement.as_str())
    }

    /// The grid budget of the tab's results.
    #[must_use]
    pub fn budget(&self) -> GridBudget {
        self.budget
    }

    /// Whether the driver of the session declares manual commit (commit / roll back offered).
    #[must_use]
    pub fn offers_transactions(&self) -> bool {
        self.session()
            .is_some_and(|s| s.info().capabilities.transactions.manual_commit)
    }

    /// The cancel button's label (I8): what the driver can do.
    #[must_use]
    pub fn cancel_label(&self) -> &'static str {
        match self.session().map(|s| s.info().capabilities.cancel) {
            Some(CancelSupport::LocalOnly) => "Stop fetching",
            _ => "Cancel",
        }
    }

    /// Runs go to `session` from now on; the editor follows its dialect.
    pub fn set_session(&mut self, session: Option<SessionHandle>, cx: &mut Context<Self>) {
        let connection = session.as_ref().map(|s| s.info().connection);
        let setup = setup_for(session.as_ref());
        self.transaction = session.as_ref().map(SessionHandle::transaction_state);
        self.session = session;
        self.message = None;
        self.editor.update(cx, |e, cx| {
            e.set_setup(setup, cx);
            e.set_connection(connection, cx);
            e.set_notice(None, cx);
        });
        cx.notify();
    }

    /// Whether closing this tab must ask first: its session has an open transaction and the
    /// preferences ask (always for production, ADR-0007 §5).
    #[must_use]
    pub fn close_needs_confirmation(&self, cx: &App) -> bool {
        let prefs = self.hub.read(cx).preferences();
        self.session()
            .is_some_and(|s| s.close_needs_confirmation(&prefs.confirmations))
    }

    fn on_hub_event(&mut self, _: Entity<ConnectionHub>, event: &HubEvent, cx: &mut Context<Self>) {
        if let HubEvent::TransactionChanged { session, state } = event
            && self.session_id() == Some(*session)
        {
            self.transaction = Some(*state);
            cx.notify();
        }
    }

    fn session_id(&self) -> Option<SessionId> {
        self.session.as_ref().map(SessionHandle::id)
    }

    fn say(&mut self, text: impl Into<SharedString>, error: bool, cx: &mut Context<Self>) {
        self.message = Some(TabMessage {
            text: text.into(),
            error,
        });
        cx.notify();
    }

    // --- execution ---------------------------------------------------------------------------

    /// Starts the run of an editor request (from the editor's sink).
    pub fn start(&mut self, request: ExecutionRequest, window: &mut Window, cx: &mut Context<Self>) {
        let id = request.id;
        let refuse = |this: &mut Self, why: String, cx: &mut Context<Self>| {
            this.editor.update(cx, |e, cx| {
                e.set_notice(Some(why.into()), cx);
                e.finish_run(id, cx);
            });
        };
        if self.run.is_some() {
            return refuse(
                self,
                "A run is already going on in this editor: cancel it first.".to_owned(),
                cx,
            );
        }
        let Some(session) = self.session().cloned() else {
            return refuse(
                self,
                "No open connection for this editor: connect, then choose it (Choose editor connection)."
                    .to_owned(),
                cx,
            );
        };
        let Some(service) = self.hub.update(cx, |h, _| h.execution()) else {
            return refuse(
                self,
                "The driver runtime is not running: statements cannot run.".to_owned(),
                cx,
            );
        };
        let document = self.editor.read(cx).text(cx);
        let fetch = service.preferences().execution.budget();
        let template =
            match run::build_run_request(&request, &document, fetch, self.budget.page_rows) {
                Ok(t) => t,
                Err(why) => return refuse(self, why, cx),
            };
        self.message = None;
        self.grid(ExecutionEvent::Started, window, cx);
        self.run = Some(ActiveRun {
            request,
            template: template.clone(),
            base: 0,
            spans: HashMap::new(),
            confirmation: None,
            generation: 0,
            task: None,
        });
        self.spawn_run(&service, &session, template, window, cx);
    }

    fn spawn_run(
        &mut self,
        service: &ninesql_core::ExecutionService,
        session: &SessionHandle,
        request: RunRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.generation += 1;
        let generation = self.generation;
        let Some(active) = self.run.as_mut() else {
            return;
        };
        // Never blocks: the core runs the statements on the driver runtime.
        let run = service.run(session, request);
        *self.current.borrow_mut() = Some(CurrentRun {
            request: active.request.id,
            control: run.control(),
            base: active.base,
        });
        active.generation = generation;
        active.spans.clear();
        let task = cx.spawn_in(window, async move |this, cx| {
            let mut run = run;
            loop {
                // Awaited on the background executor; applied on the foreground thread.
                let (back, event) = cx
                    .background_spawn(async move {
                        let event = run.next().await;
                        (run, event)
                    })
                    .await;
                run = back;
                let Some(event) = event else { break };
                let done = matches!(event, RunEvent::Done(_));
                let applied = this.update_in(cx, |tab, window, cx| {
                    tab.on_run_event(generation, event, window, cx);
                });
                if applied.is_err() || done {
                    break;
                }
            }
        });
        if let Some(active) = self.run.as_mut() {
            active.task = Some(task);
        }
    }

    fn grid(&mut self, event: ExecutionEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.results.update(cx, |r, cx| r.apply(event, window, cx));
    }

    /// Applies one event of the core run `generation`.
    fn on_run_event(
        &mut self,
        generation: u64,
        event: RunEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(active) = self.run.as_mut().filter(|r| r.generation == generation) else {
            return;
        };
        let base = active.base;
        let n = |index: usize| base.saturating_add(u32::try_from(index).unwrap_or(u32::MAX));
        match event {
            RunEvent::StatementStarted { index, span, .. } => {
                active.spans.insert(index, span);
                self.grid(
                    ExecutionEvent::StatementStarted { statement: n(index) },
                    window,
                    cx,
                );
            }
            RunEvent::WaitingForPreviousCancel { .. } => {
                self.say("Waiting for the previous statement to stop…", false, cx);
            }
            RunEvent::Item { index, item } => self.grid(
                ExecutionEvent::Item {
                    statement: n(index),
                    item,
                },
                window,
                cx,
            ),
            RunEvent::StatementClosed { index, .. } => self.grid(
                ExecutionEvent::StatementClosed { statement: n(index) },
                window,
                cx,
            ),
            RunEvent::StatementFinished {
                index, transaction, ..
            } => {
                self.transaction = Some(transaction);
                self.grid(
                    ExecutionEvent::StatementFinished { statement: n(index) },
                    window,
                    cx,
                );
            }
            RunEvent::StatementFailed {
                index,
                error,
                position,
                ..
            } => {
                let message = run::core_error_text(&error);
                self.report(index, message.clone(), run::error_code(&error), position, cx);
                self.grid(
                    ExecutionEvent::StatementFailed {
                        statement: n(index),
                        message,
                    },
                    window,
                    cx,
                );
            }
            RunEvent::StatementCancelled { .. } => {
                self.say("Cancelled.", false, cx);
                self.grid(ExecutionEvent::Cancelled, window, cx);
            }
            RunEvent::StatementTimedOut { index, after } => {
                let message = format!(
                    "The statement exceeded its timeout ({} s) and was cancelled.",
                    after.as_secs()
                );
                self.report(index, message.clone(), None, None, cx);
                self.grid(
                    ExecutionEvent::StatementFailed {
                        statement: n(index),
                        message,
                    },
                    window,
                    cx,
                );
            }
            RunEvent::NeedsConfirmation {
                index,
                span,
                request,
                script_state,
                ..
            } => {
                active.confirmation = Some(PendingConfirmation {
                    index,
                    span,
                    request,
                    script_state,
                });
            }
            RunEvent::Done(summary) => {
                self.transaction = Some(summary.transaction);
                let confirm = matches!(summary.end, RunEnd::NeedsConfirmation { .. });
                match self.run.as_ref().and_then(|r| r.confirmation.clone()) {
                    Some(pending) if confirm => self.ask_confirmation(pending, window, cx),
                    _ => self.end_run(window, cx),
                }
            }
            // `Started`, `PageEnd` (the grid counts rows itself) and future events.
            _ => {}
        }
    }

    /// Places an error of statement `index` of the current core run in the editor.
    fn report(
        &mut self,
        index: usize,
        message: String,
        code: Option<String>,
        position: Option<ninesql_core::ErrorPosition>,
        cx: &mut Context<Self>,
    ) {
        let Some(active) = self.run.as_ref() else {
            return;
        };
        let start = active
            .spans
            .get(&index)
            .map(|s| s.start)
            .or(if index == 0 {
                active.template.resume_from
            } else {
                None
            });
        let (item, location) = run::locate_error(&active.request, start, position.as_ref());
        let error = StatementError {
            request: active.request.id,
            item,
            message,
            code,
            location,
        };
        self.editor.update(cx, |e, cx| e.report_error(&error, cx));
    }

    /// The run ended: the grid and the editor are told, bind values are dropped.
    fn end_run(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(active) = self.run.take() else {
            return;
        };
        *self.current.borrow_mut() = None;
        self.grid(ExecutionEvent::Finished, window, cx);
        let id = active.request.id;
        self.editor.update(cx, |e, cx| e.finish_run(id, cx));
        cx.notify();
    }

    fn ask_confirmation(
        &mut self,
        pending: PendingConfirmation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let bidi = self.run.as_ref().map_or(0, |r| r.request.bidi_controls);
        let view = StatementConfirmView::new(&pending.request, bidi);
        let color = self
            .type_color(cx)
            .unwrap_or_else(|| hsla(ninesql_core::ConnectionType::production().color));
        let confirm: crate::connections::connect_flow::ConfirmCallback = {
            let this = cx.entity().downgrade();
            Rc::new(move |window: &mut Window, cx: &mut App| {
                let _ = this.update(cx, |tab, cx| tab.confirm(window, cx));
            })
        };
        let cancel: crate::connections::connect_flow::ConfirmCallback = {
            let this = cx.entity().downgrade();
            Rc::new(move |window: &mut Window, cx: &mut App| {
                let _ = this.update(cx, |tab, cx| tab.decline(window, cx));
            })
        };
        open_statement_confirmation(view, color, confirm, cancel, window, cx);
        cx.notify();
    }

    /// "Run" in the confirmation: resumes the run at the confirmed statement with its token.
    pub fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pending) = self.run.as_mut().and_then(|r| r.confirmation.take()) else {
            return;
        };
        let Some(session) = self.session().cloned() else {
            self.say("The session closed: nothing was run.", true, cx);
            return self.end_run(window, cx);
        };
        let Some(service) = self.hub.update(cx, |h, _| h.execution()) else {
            return self.end_run(window, cx);
        };
        let Some(active) = self.run.as_mut() else {
            return;
        };
        active.base = active
            .base
            .saturating_add(u32::try_from(pending.index).unwrap_or(u32::MAX));
        let request = active
            .template
            .clone()
            .resume_from(pending.span.start)
            .with_script_state(pending.script_state)
            .confirmed(pending.request.token);
        self.spawn_run(&service, &session, request, window, cx);
    }

    /// "Cancel" in the confirmation: the run stops before the statement.
    pub fn decline(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.awaiting_confirmation() {
            self.say("Not run: the statement was not confirmed.", false, cx);
            self.end_run(window, cx);
        }
    }

    /// Cancels the running statement ("Cancel" / "Stop fetching", F-SQL-05). A run waiting for
    /// a confirmation stops there.
    pub fn cancel_run(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.awaiting_confirmation() {
            return self.decline(window, cx);
        }
        if let Some(run) = self.current.borrow().as_ref() {
            run.control.cancel();
        }
    }

    // --- transactions ------------------------------------------------------------------------

    /// Commits (`commit = true`) or rolls back the session's transaction.
    pub fn end_transaction(&mut self, commit: bool, cx: &mut Context<Self>) {
        let Some(session) = self.session().cloned() else {
            return self.say("No open connection for this editor.", false, cx);
        };
        if !self.offers_transactions() {
            return self.say(
                "This driver does not offer manual transactions: every statement is committed on its own.",
                false,
                cx,
            );
        }
        if !session.transaction_state().in_transaction {
            return self.say("No transaction is open on this session.", false, cx);
        }
        let reply = if commit {
            session.commit()
        } else {
            session.rollback()
        };
        cx.spawn(async move |this, cx| {
            let result = reply.await;
            let _ = this.update(cx, |tab, cx| match result {
                Some(Ok(state)) => {
                    tab.transaction = Some(state);
                    tab.say(
                        if commit {
                            "Transaction committed."
                        } else {
                            "Transaction rolled back."
                        },
                        false,
                        cx,
                    );
                }
                Some(Err(e)) => tab.say(run::core_error_text(&e), true, cx),
                None => tab.say(run::core_error_text(&CoreError::SessionClosed), true, cx),
            });
        })
        .detach();
    }

    // --- rendering ---------------------------------------------------------------------------

    fn badges(&self, cx: &App) -> Option<ConnectionBadges> {
        let session = self.session()?;
        let hub = self.hub.read(cx);
        let c = hub.connection(session.info().connection)?;
        Some(ConnectionBadges::of(
            c,
            hub.types(),
            std::slice::from_ref(session),
        ))
    }

    /// The color of the session's connection type when production rules apply (frame and
    /// confirmation, ADR-0007 §4).
    #[must_use]
    pub fn frame_color(&self, cx: &App) -> Option<Hsla> {
        self.badges(cx).filter(|b| b.production).map(|b| b.color)
    }

    fn type_color(&self, cx: &App) -> Option<Hsla> {
        self.badges(cx).map(|b| b.color)
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let theme = cx.theme();
        let bg = theme.background;
        let contrast = |c: Hsla| {
            crate::appearance::ensure_contrast(c, &[bg], crate::appearance::MIN_TEXT_CONTRAST)
        };
        let muted = contrast(theme.muted_foreground);
        let danger = contrast(theme.danger);
        let warning = contrast(theme.warning);
        let border = theme.border;
        let label = match self.session() {
            Some(s) => format!("Connection: {}", s.info().connection_name),
            None => "No connection".to_owned(),
        };
        let running = self.is_running();
        let in_tx = self.transaction().is_some_and(|t| t.in_transaction);
        let auto_commit = self.transaction().map(|t| t.auto_commit);
        let this = cx.entity().downgrade();
        let mut bar = h_flex()
            .id("editor-toolbar")
            .gap_2()
            .px_2()
            .py_1()
            .text_xs()
            .border_b_1()
            .border_color(border)
            .child(
                Button::new("editor-connection")
                    .xsmall()
                    .ghost()
                    .label(label)
                    .tooltip("Choose editor connection")
                    .on_click(move |_, window, cx| {
                        window.dispatch_action(
                            Box::new(crate::actions::ChooseEditorConnection),
                            cx,
                        );
                    }),
            );
        if let Some(b) = self.badges(cx) {
            bar = bar.child(badges("editor", &b, true, cx));
        }
        if let Some(auto) = auto_commit {
            bar = bar.child(
                div()
                    .id("editor-transaction")
                    .text_color(if in_tx { warning } else { muted })
                    .child(match (auto, in_tx) {
                        (_, true) => "Transaction open",
                        (true, false) => "Auto-commit",
                        (false, false) => "Manual commit",
                    }),
            );
        }
        if self.offers_transactions() {
            let commit = this.clone();
            let rollback = this.clone();
            bar = bar
                .child(
                    Button::new("editor-commit")
                        .xsmall()
                        .label("Commit")
                        .disabled(!in_tx || running)
                        .on_click(move |_, _, cx| {
                            let _ = commit.update(cx, |t, cx| t.end_transaction(true, cx));
                        }),
                )
                .child(
                    Button::new("editor-rollback")
                        .xsmall()
                        .label("Roll back")
                        .disabled(!in_tx || running)
                        .on_click(move |_, _, cx| {
                            let _ = rollback.update(cx, |t, cx| t.end_transaction(false, cx));
                        }),
                );
        }
        if running {
            let cancel = this.clone();
            bar = bar.child(
                Button::new("editor-cancel")
                    .xsmall()
                    .danger()
                    .label(self.cancel_label())
                    .tooltip("Cancel execution")
                    .on_click(move |_, window, cx| {
                        let _ = cancel.update(cx, |t, cx| t.cancel_run(window, cx));
                    }),
            );
        }
        if let Some(m) = &self.message {
            bar = bar.child(
                div()
                    .id("editor-message")
                    .text_color(if m.error { danger } else { muted })
                    .child(m.text.clone()),
            );
        }
        bar
    }
}

/// What runs when the user chose how to end an open transaction: `true` commits, `false`
/// rolls back.
pub type CloseCallback = Rc<dyn Fn(bool, &mut Window, &mut App)>;

/// Asks how to end the open transaction of a session about to be closed (ADR-0007 §5): commit,
/// roll back, or keep it open (Cancel, Escape, the close button).
pub fn open_close_confirmation(
    connection_name: String,
    on_choice: CloseCallback,
    window: &mut Window,
    cx: &mut App,
) {
    window.open_dialog(cx, move |dialog, _window, _cx| {
        let commit = {
            let on_choice = on_choice.clone();
            Button::new("close-commit")
                .primary()
                .label("Commit and close")
                .on_click(move |_, window, cx| {
                    window.close_dialog(cx);
                    on_choice(true, window, cx);
                })
        };
        let rollback = {
            let on_choice = on_choice.clone();
            Button::new("close-rollback")
                .danger()
                .label("Roll back and close")
                .on_click(move |_, window, cx| {
                    window.close_dialog(cx);
                    on_choice(false, window, cx);
                })
        };
        let cancel = Button::new("close-cancel")
            .label("Cancel")
            .on_click(|_, window, cx| window.close_dialog(cx));
        dialog
            .title("Uncommitted transaction")
            .w(px(520.))
            .child(div().text_sm().child(format!(
                "A transaction with uncommitted changes is open on \"{connection_name}\". Commit it, or roll it back, before closing?"
            )))
            .footer(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(cancel)
                    .child(rollback)
                    .child(commit),
            )
    });
}

/// The connection picker of an editor: one button per open session, and "No connection".
pub fn open_connection_picker(
    editor: Entity<SqlEditorTab>,
    sessions: Vec<SessionHandle>,
    current: Option<SessionId>,
    window: &mut Window,
    cx: &mut App,
) {
    let sessions = Rc::new(sessions);
    window.open_dialog(cx, move |dialog, _window, cx| {
        let muted = cx.theme().muted_foreground;
        let rows = sessions.iter().enumerate().map(|(i, s)| {
            let editor = editor.clone();
            let session = s.clone();
            let info = s.info();
            let mut label = info.connection_name.clone();
            if info.read_only {
                label.push_str(" (read-only)");
            }
            if info.policy.production {
                label.push_str(" (production)");
            }
            Button::new(("editor-pick", i))
                .label(label)
                .when(current == Some(s.id()), |b| b.primary())
                .on_click(move |_, window, cx| {
                    window.close_dialog(cx);
                    let session = session.clone();
                    editor.update(cx, |e, cx| e.set_session(Some(session), cx));
                })
        });
        let none = {
            let editor = editor.clone();
            Button::new("editor-pick-none")
                .ghost()
                .label("No connection")
                .on_click(move |_, window, cx| {
                    window.close_dialog(cx);
                    editor.update(cx, |e, cx| e.set_session(None, cx));
                })
        };
        dialog
            .title("Choose editor connection")
            .w(px(480.))
            .child(
                v_flex()
                    .gap_2()
                    .text_sm()
                    .when(sessions.is_empty(), |el| {
                        el.child(div().text_color(muted).child(
                            "No connection is open: connect from the navigator first (Connect).",
                        ))
                    })
                    .children(rows)
                    .child(none),
            )
    });
}

impl Focusable for SqlEditorTab {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Render for SqlEditorTab {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let toolbar = self.render_toolbar(cx);
        v_flex().size_full().child(toolbar).child(
            div().flex_1().min_h_0().child(
                v_resizable("editor-results")
                    .child(resizable_panel().child(self.editor.clone()))
                    .child(resizable_panel().size(px(260.)).child(self.results.clone())),
            ),
        )
    }
}

#[cfg(test)]
mod tests;
