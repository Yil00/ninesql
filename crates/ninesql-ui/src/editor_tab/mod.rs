//! An SQL editor tab (P3): `ninesql_editor::SqlEditor` above the results grid
//! (`ninesql_grid::ResultsView`), on one open session chosen with the connection picker.
//!
//! - **Setup.** The editor follows the session's dialect: `Dialect::split_script` and the shared
//!   lexer for splitting and segments, `ParameterStyle::for_engine`, `KeywordSet::for_engine` and
//!   `Dialect::quote_identifier` for completion. Objects and columns come from the metadata
//!   cache through `browse::completion::MetadataCompletion` (never loading, I6); when the tab
//!   binds to a connection it resolves the default schema (the connection's `default_schema`,
//!   else the first schema the driver reports) and warms it (`warm_schema`), cancelled when the
//!   tab rebinds or closes.
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
//!   "Cancel" stops the run there. A run is bound to the session it started on: resumes go
//!   there only (a closed session ends the run), the connection picker is disabled and
//!   `set_session` refuses while it lasts, and the dialog's color comes from that session. A
//!   dialog that goes away without an answer ends the run and drops its bind values.
//! - **Transactions.** Commit and roll back are refused while a run lasts, so they never land
//!   between two statements of a script.
//! - **Transactions (F-SQL-07).** The indicator follows `CoreEvent::TransactionChanged`
//!   (through the hub) and the state reported after each statement; commit and roll back are
//!   offered only when the driver declares manual commit (I8).
//! - **Cancel (F-SQL-05).** `CancelExecution` (editor) and the toolbar's button cancel the run;
//!   the button says "Cancel" only when the driver declares a server-side cancel
//!   (`CancelSupport::ServerSide`), "Stop fetching" otherwise (`LocalOnly`, or a kind added
//!   later).
//! - **Idle paused results (ADR-0024 §5).** `RunEvent::ResultClosedIdle` goes to the grid (its
//!   result says it was closed, fetching is off for it) and to the toolbar, with a hint when the
//!   session's transaction is still open.

pub mod edit_dialogs;
pub mod run;
pub mod table;

use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::base::async_util::{Sender, unbounded};
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
use ninesql_editor::completion::{CompletionEngine, KeywordSet, MetadataProvider, NoMetadata};
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

/// The editor setup for `session` (its dialect), or ANSI without a session, with object
/// completion from `metadata` (keywords, aliases and CTE names only without it).
#[must_use]
pub fn setup_for(
    session: Option<&SessionHandle>,
    metadata: Option<Rc<dyn MetadataProvider>>,
) -> EditorSetup {
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
        metadata: metadata.unwrap_or_else(|| Rc::new(NoMetadata)),
    }
}

/// The completion provider of a tab bound to `session`: the metadata cache of its connection.
fn completion_provider(
    hub: &Entity<ConnectionHub>,
    session: Option<&SessionHandle>,
    default_schema: &Rc<RefCell<Option<String>>>,
    cx: &App,
) -> Option<Rc<dyn MetadataProvider>> {
    let session = session?;
    let service = hub.read(cx).metadata()?;
    Some(Rc::new(crate::browse::completion::MetadataCompletion::new(
        service,
        session.info().connection,
        default_schema.clone(),
    )))
}

/// The cancel button's label for a driver's [`CancelSupport`] (I8): "Cancel" only when the
/// server stops the statement; "Stop fetching" otherwise, a support kind added later included
/// (`CancelSupport` is non-exhaustive: an unknown kind never promises a server-side cancel).
#[must_use]
pub fn cancel_label_for(support: Option<CancelSupport>) -> &'static str {
    match support {
        Some(CancelSupport::ServerSide) | None => "Cancel",
        Some(_) => "Stop fetching",
    }
}

/// The toolbar message after the core closed a paused result left idle (ADR-0024 §5), with the
/// open-transaction hint when the session still has one.
#[must_use]
pub fn idle_close_message(after: std::time::Duration, in_transaction: bool) -> String {
    let mut text = format!(
        "Result closed after {} idle: the rows already loaded are kept. Run the statement again to read more.",
        ninesql_grid::view::idle_duration_text(after.as_secs())
    );
    if in_transaction {
        text.push_str(
            " The transaction is still open on this session: commit or roll back when you are done.",
        );
    }
    text
}

/// The text of the editor item a core statement span belongs to, for "Export results…";
/// `None` when it had bind values (they are not kept after the run) or no item matches.
fn statement_text(request: &ExecutionRequest, span: &Range<usize>) -> Option<String> {
    let item = request
        .items
        .iter()
        .find(|i| i.span.start <= span.start && span.start < i.span.end.max(i.span.start + 1))?;
    item.params.is_empty().then(|| item.text.clone())
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
    /// The session the run started on: every core run of it (resumes after a confirmation
    /// included) goes there and nowhere else (ADR-0007: what was confirmed is what runs, where
    /// the dialog said).
    session: SessionHandle,
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

impl Drop for ActiveRun {
    /// The run's bind values (in the editor's request and in the core request kept for a
    /// resume) are zeroized when the run ends, whichever way (phase 3 review L3).
    fn drop(&mut self) {
        for item in &mut self.request.items {
            run::wipe_params(&mut item.params);
        }
        run::wipe_params(&mut self.template.params);
    }
}

/// What the toolbar shows about the last action (errors of transaction commands, refusals).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TabMessage {
    /// Text.
    pub text: SharedString,
    /// An error (shown in the danger color) or an info.
    pub error: bool,
}

/// Dropped with the statement confirmation dialog (its builder owns it): tells the tab the
/// dialog is gone, so a run whose dialog vanished without an answer (closed by other code,
/// replaced, its window closed) ends and drops its bind values instead of waiting forever.
struct ConfirmationGuard {
    generation: u64,
    dropped: Sender<u64>,
}

impl Drop for ConfirmationGuard {
    fn drop(&mut self) {
        let _ = self.dropped.try_send(self.generation);
    }
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
    /// Where [`ConfirmationGuard`]s report their dialog gone.
    confirmation_dropped: Sender<u64>,
    /// Text of each grid statement of the last run, for "Export results…" (`None`: it had bind
    /// values, which are not kept after the run).
    statement_texts: HashMap<u32, Option<String>>,
    /// The default schema completion resolves unqualified names in, once known.
    default_schema: Rc<RefCell<Option<String>>>,
    /// Resolves the default schema and warms it; dropped (cancelled) on rebind.
    warming: Option<Task<()>>,
    /// The table the tab shows and its editing (P5-A).
    table: table::TableState,
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
        let default_schema: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
        let setup = setup_for(
            session.as_ref(),
            completion_provider(&hub, session.as_ref(), &default_schema, cx),
        );
        let connection = session.as_ref().map(|s| s.info().connection);
        let sink = Rc::new(TabSink::new(tx, current.clone()));
        let editor = cx.new(|cx| {
            let mut editor = SqlEditor::new(setup, window, cx);
            editor.set_sink(sink);
            editor.set_connection(connection, cx);
            editor
        });
        let source = Rc::new(RunSource::new(current.clone()));
        let dialect = session.as_ref().map(SessionHandle::dialect);
        let results = cx.new(|cx| {
            let mut view = ResultsView::new(budget, cx);
            view.set_source(source);
            view.set_dialect(dialect);
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
        let (dropped_tx, dropped_rx) = unbounded::<u64>();
        let dropped = cx.spawn_in(window, async move |this, cx| {
            while let Ok(generation) = dropped_rx.recv().await {
                if this
                    .update_in(cx, |tab, window, cx| {
                        tab.on_confirmation_dropped(generation, window, cx);
                    })
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
            confirmation_dropped: dropped_tx,
            statement_texts: HashMap::new(),
            default_schema,
            warming: None,
            table: table::TableState::default(),
            _tasks: vec![requests, dropped],
            _subscriptions: subscriptions,
        }
        .warmed(cx)
    }

    fn warmed(mut self, cx: &mut Context<Self>) -> Self {
        self.warm_metadata(cx);
        self
    }

    /// Resolves the default schema of the tab's connection and warms it for completion
    /// (`warm_schema`). Only awaits metadata requests (I6); replaced on rebind.
    fn warm_metadata(&mut self, cx: &mut Context<Self>) {
        self.warming = None;
        *self.default_schema.borrow_mut() = None;
        let Some(session) = self.session().cloned() else {
            return;
        };
        let hub = self.hub.read(cx);
        let Some(service) = hub.metadata() else {
            return;
        };
        let connection = session.info().connection;
        let preferred = hub
            .connection(connection)
            .and_then(|c| c.default_schema.clone());
        let slot = self.default_schema.clone();
        self.warming = Some(cx.spawn(async move |_, _| {
            let Some(schema) = crate::browse::completion::resolve_default_schema(
                &service,
                connection,
                preferred.as_deref(),
            )
            .await
            else {
                return;
            };
            *slot.borrow_mut() = schema
                .leaf()
                .map(|s| s.name.clone())
                .filter(|n| crate::browse::completion::insertable(n));
            // Held by this task: dropping the task (rebind, close) cancels the prefetch.
            let _summary = service.warm_schema(connection, schema).await;
        }));
    }

    /// The default schema completion uses, once resolved (tests).
    #[must_use]
    pub fn default_schema(&self) -> Option<String> {
        self.default_schema.borrow().clone()
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
        cancel_label_for(self.session().map(|s| s.info().capabilities.cancel))
    }

    /// The session the current run started on (where it goes on after a confirmation).
    #[must_use]
    pub fn run_session(&self) -> Option<&SessionHandle> {
        self.run.as_ref().map(|r| &r.session)
    }

    /// Runs go to `session` from now on; the editor follows its dialect. Refused while a run
    /// is going on (a statement waiting for confirmation included): the rest of the run must
    /// go to the session it started on. Returns whether the session changed.
    pub fn set_session(&mut self, session: Option<SessionHandle>, cx: &mut Context<Self>) -> bool {
        if self.run.is_some() {
            self.say(
                "A run is going on in this editor: cancel it before choosing another connection.",
                true,
                cx,
            );
            return false;
        }
        if self.has_pending_edits() || self.table.saving || self.table.confirmation.is_some() {
            self.say(
                "There are unsaved changes in the result: save or discard them before choosing another connection.",
                true,
                cx,
            );
            return false;
        }
        // The table (and its key) belonged to the previous session.
        self.table = table::TableState::default();
        let connection = session.as_ref().map(|s| s.info().connection);
        let setup = setup_for(
            session.as_ref(),
            completion_provider(&self.hub, session.as_ref(), &self.default_schema, cx),
        );
        self.transaction = session.as_ref().map(SessionHandle::transaction_state);
        let dialect = session.as_ref().map(SessionHandle::dialect);
        self.results.update(cx, |r, _| r.set_dialect(dialect));
        self.session = session;
        self.message = None;
        self.editor.update(cx, |e, cx| {
            e.set_setup(setup, cx);
            e.set_connection(connection, cx);
            e.set_notice(None, cx);
        });
        self.warm_metadata(cx);
        cx.notify();
        true
    }

    /// The statement of the active result, for "Export results…".
    ///
    /// # Errors
    /// A user-facing reason: no result, a run going on, bind values.
    pub fn export_statement(&self, cx: &App) -> Result<String, &'static str> {
        if self.is_running() {
            return Err("Wait for the run to end before exporting its results.");
        }
        let statement = self
            .results
            .read(cx)
            .active_statement()
            .ok_or("Run a query first: Export results exports the active result.")?;
        match self.statement_texts.get(&statement) {
            Some(Some(text)) => Ok(text.clone()),
            Some(None) => Err(
                "This statement had bind parameters, which are not kept after the run: export it with the values written in the statement.",
            ),
            None => Err("The statement of this result is not known any more: run it again."),
        }
    }

    /// "Format SQL": formats the selection, or the whole script, with the session's dialect
    /// (ANSI without a session). Refused during a run. Returns whether the text changed.
    pub fn format_sql(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.is_running() {
            self.say("Wait for the run to end before formatting.", true, cx);
            return false;
        }
        let dialect: Arc<dyn ninesql_core::driver_api::Dialect> = match self.session() {
            Some(s) => s.dialect(),
            None => Arc::new(AnsiDialect),
        };
        let options = ninesql_core::FormatOptions::default();
        self.editor.update(cx, |e, cx| {
            e.transform_selection_or_all(
                |text| ninesql_core::format_sql(dialect.as_ref(), text, &options),
                window,
                cx,
            )
        })
    }

    /// Inserts a snippet expansion at the cursor (in place of the selection).
    pub fn insert_snippet_text(
        &mut self,
        text: &str,
        cursor: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor
            .update(cx, |e, cx| e.insert_text(text, cursor, window, cx));
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
    pub fn start(
        &mut self,
        request: ExecutionRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
        if self.has_pending_edits() {
            return refuse(
                self,
                "There are unsaved changes in the result: save or discard them first (Save changes / Discard changes)."
                    .to_owned(),
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
        let keep = self.table.keep_message && self.table.source_request == Some(request.id);
        self.table.keep_message = false;
        if !keep {
            self.message = None;
        }
        self.statement_texts.clear();
        self.table_run_started(request.id);
        self.grid(ExecutionEvent::Started, window, cx);
        self.run = Some(ActiveRun {
            session,
            request,
            template: template.clone(),
            base: 0,
            spans: HashMap::new(),
            confirmation: None,
            generation: 0,
            task: None,
        });
        self.spawn_run(&service, template, window, cx);
    }

    /// Why the run's session can no longer take the run, if so: it closed, or the tab's
    /// session is no longer the one the run started on.
    fn run_session_problem(&self) -> Option<&'static str> {
        let active = self.run.as_ref()?;
        if active.session.is_closed() {
            Some("The session of this run closed: the rest of the run was not run.")
        } else if self.session_id() != Some(active.session.id()) {
            Some("The editor's connection changed during the run: the rest of the run was not run.")
        } else {
            None
        }
    }

    /// Starts a core run of the active run, on the session the run started on.
    fn spawn_run(
        &mut self,
        service: &ninesql_core::ExecutionService,
        request: RunRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(why) = self.run_session_problem() {
            self.say(why, true, cx);
            return self.end_run(window, cx);
        }
        self.generation += 1;
        let generation = self.generation;
        let Some(active) = self.run.as_mut() else {
            return;
        };
        // Never blocks: the core runs the statements on the driver runtime.
        let run = service.run(&active.session, request);
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
                let text = statement_text(&active.request, &span);
                self.statement_texts.insert(n(index), text);
                active.spans.insert(index, span);
                self.grid(
                    ExecutionEvent::StatementStarted {
                        statement: n(index),
                    },
                    window,
                    cx,
                );
            }
            RunEvent::WaitingForPreviousCancel { .. } => {
                self.say("Waiting for the previous statement to stop…", false, cx);
            }
            RunEvent::Item { index, item } => {
                self.grid(
                    ExecutionEvent::Item {
                        statement: n(index),
                        item,
                    },
                    window,
                    cx,
                );
                self.refresh_editing(cx);
            }
            RunEvent::ResultClosedIdle { index, rows, after } => {
                // The session's own state: a paused read may sit inside an open transaction.
                let in_tx = self
                    .session()
                    .map(SessionHandle::transaction_state)
                    .or(self.transaction)
                    .is_some_and(|t| t.in_transaction);
                self.say(idle_close_message(after, in_tx), in_tx, cx);
                self.grid(
                    ExecutionEvent::ResultClosedIdle {
                        statement: n(index),
                        rows,
                        after,
                    },
                    window,
                    cx,
                );
            }
            RunEvent::StatementClosed { index, .. } => self.grid(
                ExecutionEvent::StatementClosed {
                    statement: n(index),
                },
                window,
                cx,
            ),
            RunEvent::StatementFinished {
                index, transaction, ..
            } => {
                self.transaction = Some(transaction);
                self.grid(
                    ExecutionEvent::StatementFinished {
                        statement: n(index),
                    },
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
                self.report(
                    index,
                    message.clone(),
                    run::error_code(&error),
                    position,
                    cx,
                );
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
        let start = active.spans.get(&index).map(|s| s.start).or(if index == 0 {
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
        self.refresh_editing(cx);
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
        let Some(active) = self.run.as_ref() else {
            return;
        };
        let bidi = active.request.bidi_controls;
        let view = StatementConfirmView::new(&pending.request, bidi);
        // The color of the session the statement will run on (the run's), never another one.
        let badges = ConnectionBadges::of_open_session(self.hub.read(cx), &active.session);
        let color = if pending.request.production && !badges.production {
            hsla(ninesql_core::ConnectionType::production().color)
        } else {
            badges.color
        };
        let guard: Rc<dyn std::any::Any> = Rc::new(ConfirmationGuard {
            generation: active.generation,
            dropped: self.confirmation_dropped.clone(),
        });
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
        open_statement_confirmation(view, color, confirm, cancel, Some(guard), window, cx);
        cx.notify();
    }

    /// The confirmation dialog of core run `generation` is gone: if it was never answered, the
    /// run ends there (nothing more runs) and its bind values are dropped.
    fn on_confirmation_dropped(
        &mut self,
        generation: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if generation == edit_dialogs::SAVE_CONFIRMATION_GENERATION {
            // The save confirmation closed without an answer: nothing is saved.
            self.decline_save(cx);
            return;
        }
        let unanswered = self
            .run
            .as_ref()
            .is_some_and(|r| r.generation == generation && r.confirmation.is_some());
        if unanswered {
            self.say(
                "Not run: the confirmation closed without an answer.",
                false,
                cx,
            );
            self.end_run(window, cx);
        }
    }

    /// The type color and badges of the session the dialog of the current run shows (tests).
    #[cfg(test)]
    fn run_badges(&self, cx: &App) -> Option<ConnectionBadges> {
        let active = self.run.as_ref()?;
        Some(ConnectionBadges::of_open_session(
            self.hub.read(cx),
            &active.session,
        ))
    }

    /// "Run" in the confirmation: resumes the run at the confirmed statement with its token.
    pub fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pending) = self.run.as_mut().and_then(|r| r.confirmation.take()) else {
            return;
        };
        if let Some(why) = self.run_session_problem() {
            self.say(
                format!("{why} The confirmed statement was not run."),
                true,
                cx,
            );
            return self.end_run(window, cx);
        }
        let Some(service) = self.hub.update(cx, |h, _| h.execution()) else {
            self.say(
                "The driver runtime is not running: nothing was run.",
                true,
                cx,
            );
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
        self.spawn_run(&service, request, window, cx);
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

    /// Commits (`commit = true`) or rolls back the session's transaction. Refused while a run
    /// is going on (between two statements of a script, or at a confirmation): the transaction
    /// command would land in the middle of the run. The task resolves to `Ok` once the
    /// transaction ended (or when there was none to end), to the error shown otherwise.
    pub fn end_transaction(
        &mut self,
        commit: bool,
        cx: &mut Context<Self>,
    ) -> Task<Result<(), String>> {
        let refuse = |this: &mut Self, why: &str, cx: &mut Context<Self>| {
            this.say(why.to_owned(), true, cx);
            Task::ready(Err(why.to_owned()))
        };
        if self.is_running() {
            return refuse(
                self,
                "A run is going on in this editor: cancel it or let it finish, then commit or roll back.",
                cx,
            );
        }
        let Some(session) = self.session().cloned() else {
            self.say("No open connection for this editor.", false, cx);
            return Task::ready(Ok(()));
        };
        if !session.transaction_state().in_transaction {
            self.say("No transaction is open on this session.", false, cx);
            return Task::ready(Ok(()));
        }
        if !self.offers_transactions() {
            return refuse(
                self,
                "This driver does not offer manual transactions: every statement is committed on its own.",
                cx,
            );
        }
        let reply = if commit {
            session.commit()
        } else {
            session.rollback()
        };
        cx.spawn(async move |this, cx| {
            let result = reply.await;
            this.update(cx, |tab, cx| match result {
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
                    Ok(())
                }
                Some(Err(e)) => {
                    let text = run::core_error_text(&e);
                    tab.say(text.clone(), true, cx);
                    Err(text)
                }
                None => {
                    let text = run::core_error_text(&CoreError::SessionClosed);
                    tab.say(text.clone(), true, cx);
                    Err(text)
                }
            })
            .map_err(|_| "the editor was closed".to_owned())?
        })
    }

    // --- rendering ---------------------------------------------------------------------------

    /// Badges of the tab's session: from its saved connection, or from the session alone when
    /// the connection is gone (a production session always shows its badge and frame).
    fn badges(&self, cx: &App) -> Option<ConnectionBadges> {
        let session = self.session()?;
        Some(ConnectionBadges::of_open_session(
            self.hub.read(cx),
            session,
        ))
    }

    /// The color of the session's connection type when production rules apply (frame and
    /// confirmation, ADR-0007 §4).
    #[must_use]
    pub fn frame_color(&self, cx: &App) -> Option<Hsla> {
        self.badges(cx).filter(|b| b.production).map(|b| b.color)
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
                    .tooltip(if running {
                        "Choose editor connection (not during a run)"
                    } else {
                        "Choose editor connection"
                    })
                    .disabled(running)
                    .on_click(move |_, window, cx| {
                        window
                            .dispatch_action(Box::new(crate::actions::ChooseEditorConnection), cx);
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
                            let _ = commit.update(cx, |t, cx| t.end_transaction(true, cx).detach());
                        }),
                )
                .child(
                    Button::new("editor-rollback")
                        .xsmall()
                        .label("Roll back")
                        .disabled(!in_tx || running)
                        .on_click(move |_, _, cx| {
                            let _ =
                                rollback.update(cx, |t, cx| t.end_transaction(false, cx).detach());
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
        bar = self.render_edit_status(bar, muted, warning, cx);
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

impl SqlEditorTab {
    /// The editing part of the toolbar: the table, why the result is read-only, or the pending
    /// changes with "Save" and "Discard".
    fn render_edit_status(
        &self,
        mut bar: Stateful<Div>,
        muted: Hsla,
        warning: Hsla,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let has_result = !self.results.read(cx).results().tabs().is_empty();
        if let Some(source) = &self.table.source {
            bar = bar.child(
                div()
                    .id("editor-table")
                    .text_color(muted)
                    .child(format!("Table: {}", source.label())),
            );
        }
        if let Some(e) = self.editing() {
            let changes = e.pending.change_count();
            let busy = self.table.saving
                || self.table.confirmation.is_some()
                || self.is_running()
                || self.session().is_none_or(SessionHandle::is_leased);
            bar = bar.child(
                div()
                    .id("editor-edit-status")
                    .text_color(if changes > 0 { warning } else { muted })
                    .child(if changes > 0 {
                        format!(
                            "Editable (key: {}) · {changes} unsaved changes",
                            e.table.key
                        )
                    } else {
                        format!("Editable (key: {})", e.table.key)
                    }),
            );
            if changes > 0 {
                let save = cx.entity().downgrade();
                let discard = cx.entity().downgrade();
                bar = bar
                    .child(
                        Button::new("editor-save-changes")
                            .xsmall()
                            .primary()
                            .label("Save")
                            .tooltip("Save changes")
                            .disabled(busy)
                            .on_click(move |_, window, cx| {
                                let _ = save.update(cx, |t, cx| t.save_changes(window, cx));
                            }),
                    )
                    .child(
                        Button::new("editor-discard-changes")
                            .xsmall()
                            .label("Discard")
                            .tooltip("Discard changes")
                            .disabled(busy)
                            .on_click(move |_, _, cx| {
                                let _ = discard.update(cx, |t, cx| t.discard_changes(cx));
                            }),
                    );
            }
        } else if (has_result || self.table.source.is_some())
            && let Some(why) = self.read_only_reason()
        {
            bar = bar.child(
                div()
                    .id("editor-read-only-reason")
                    .text_color(muted)
                    .child(why.to_owned()),
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
        dialog.title("Choose editor connection").w(px(480.)).child(
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
mod table_tests;
#[cfg(test)]
mod tests;
