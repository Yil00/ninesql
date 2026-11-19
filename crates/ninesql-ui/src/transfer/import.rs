//! "Import CSV…" (F-XFER-03): the file, a preview with its issues, the column mapping, the
//! transaction mode and "skip invalid rows", then the production confirmation, progress, cancel
//! and a summary.

use std::path::PathBuf;
use std::rc::Rc;

use gpui_kit::base::async_util::unbounded;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use ninesql_core::driver_api::metadata::ObjectPath;
use ninesql_core::transfer::{ColumnMapping, CsvFormat, CsvPreview, MappedColumn, TargetColumn};
use ninesql_core::{
    CsvPreviewRequest, ImportConfirmation, ImportEnd, ImportEvent, ImportRequest, ImportSummary,
    ImportTarget, ImportToken, ImportTransaction, SessionHandle, TransferService,
};

use super::{bytes_text, error_text, session_problem};
use crate::script_view::{ScriptRows, script_view};

/// Rows per commit in "commit every N rows" mode.
pub const COMMIT_EVERY_ROWS: u64 = 1000;
/// Preview rows shown in the dialog (the preview reads more to find issues).
pub const SHOWN_PREVIEW_ROWS: usize = 20;

/// Where the dialog stands.
#[derive(Debug)]
pub enum ImportStage {
    /// Choosing the file.
    ChooseFile,
    /// Reading the preview.
    Previewing,
    /// The preview, the mapping and the options.
    Preview,
    /// The core asks for the production confirmation: its details, shown exactly.
    Confirm(Box<ImportConfirmation>),
    /// Running.
    Running {
        /// Rows inserted.
        rows: u64,
        /// File bytes read.
        bytes: u64,
        /// File size.
        file_bytes: u64,
    },
    /// Finished (completed, cancelled or failed: the summary says which).
    Done(Box<ImportSummary>),
    /// Refused before running.
    Failed(String),
}

/// The import dialog.
pub struct ImportCsvDialog {
    session: SessionHandle,
    transfer: TransferService,
    table: Vec<String>,
    /// The table's navigator path, when the dialog was opened from the navigator.
    table_path: Option<ObjectPath>,
    targets: Vec<TargetColumn>,
    /// What the core says about rolling back rows in the table ([`ImportTarget`]), read once
    /// off the UI thread when the dialog opens; `None` until it answers.
    target: Option<ImportTarget>,
    /// Why the target could not be read, if so.
    target_error: Option<String>,
    path: Entity<InputState>,
    header: bool,
    preview: Option<CsvPreview>,
    /// `mapping[target] = Some(file field)`.
    mapping: Vec<Option<usize>>,
    transaction: ImportTransaction,
    /// The commit mode of the last import started (the summary's wording follows it, not the
    /// option shown now).
    ran_with: ImportTransaction,
    skip_invalid: bool,
    stage: ImportStage,
    /// The statement of the confirmation, laid out once ([`ScriptRows`]).
    confirm_script: Option<Rc<ScriptRows>>,
    tasks: Vec<Task<()>>,
}

impl ImportCsvDialog {
    /// A dialog importing into `table` (its `targets` columns, from the metadata cache;
    /// `table_path`: its navigator path, when known). Asks the core at once, off the UI
    /// thread, whether "All or nothing" is allowed for the table ([`Self::target`]).
    pub fn new(
        session: SessionHandle,
        transfer: TransferService,
        table: Vec<String>,
        table_path: Option<ObjectPath>,
        targets: Vec<TargetColumn>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let path = cx.new(|cx| InputState::new(window, cx).placeholder("/home/me/data.csv"));
        let mapping = vec![None; targets.len()];
        // Never blocks: the core reads the table's storage on the driver runtime.
        let reply = transfer.import_target(&session, table.clone(), table_path.clone());
        let read_target = cx.spawn(async move |this, cx| {
            let result = cx.background_spawn(reply).await;
            let _ = this.update(cx, |d, cx| d.set_target(result, cx));
        });
        Self {
            session,
            transfer,
            table,
            table_path,
            target: None,
            target_error: None,
            targets,
            path,
            header: true,
            preview: None,
            mapping,
            transaction: ImportTransaction::AllOrNothing,
            ran_with: ImportTransaction::AllOrNothing,
            skip_invalid: false,
            stage: ImportStage::ChooseFile,
            confirm_script: None,
            tasks: vec![read_target],
        }
    }

    /// The core's answer about the table ([`TransferService::import_target`]): when "All or
    /// nothing" is not allowed, the mode moves to "commit every N rows" and the reason is shown.
    fn set_target(
        &mut self,
        result: Option<Result<ImportTarget, ninesql_core::TransferJobError>>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Some(Ok(target)) => {
                self.target = Some(target);
                self.target_error = None;
                if !target.all_or_nothing_allowed()
                    && self.transaction == ImportTransaction::AllOrNothing
                {
                    self.transaction = ImportTransaction::CommitEvery(COMMIT_EVERY_ROWS);
                }
            }
            Some(Err(e)) => self.target_error = Some(error_text(&e)),
            None => self.target_error = Some("The table's storage could not be read.".into()),
        }
        cx.notify();
    }

    /// What the core said about rolling back rows in the table, once it answered.
    #[must_use]
    pub fn target(&self) -> Option<ImportTarget> {
        self.target
    }

    /// The chosen commit mode.
    #[must_use]
    pub fn transaction(&self) -> ImportTransaction {
        self.transaction
    }

    /// Why "All or nothing" is not offered for this table, if it is not (I8).
    #[must_use]
    pub fn all_or_nothing_refusal(&self) -> Option<String> {
        self.target?
            .all_or_nothing_refusal()
            .map(|e| error_text(&e))
    }

    /// The stage.
    #[must_use]
    pub fn stage(&self) -> &ImportStage {
        &self.stage
    }

    /// The path input.
    #[must_use]
    pub fn path_input(&self) -> &Entity<InputState> {
        &self.path
    }

    /// The preview read last.
    #[must_use]
    pub fn preview(&self) -> Option<&CsvPreview> {
        self.preview.as_ref()
    }

    /// The mapping: for each target column, the file field it reads.
    #[must_use]
    pub fn mapping(&self) -> &[Option<usize>] {
        &self.mapping
    }

    /// The target columns.
    #[must_use]
    pub fn targets(&self) -> &[TargetColumn] {
        &self.targets
    }

    fn format(&self) -> CsvFormat {
        let mut f = CsvFormat::default();
        f.header = self.header;
        f
    }

    /// "Header row" on / off (reads the preview again).
    pub fn set_header(&mut self, header: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.header = header;
        if self.preview.is_some() {
            self.read_preview(window, cx);
        }
        cx.notify();
    }

    /// "All or nothing" (one transaction) or "commit every N rows". "All or nothing" is refused
    /// with its reason when the table cannot roll back ([`Self::all_or_nothing_refusal`]).
    pub fn set_transaction(&mut self, t: ImportTransaction, cx: &mut Context<Self>) {
        if t == ImportTransaction::AllOrNothing
            && let Some(why) = self.all_or_nothing_refusal()
        {
            self.stage = ImportStage::Failed(why);
            cx.notify();
            return;
        }
        self.transaction = t;
        cx.notify();
    }

    /// "Skip invalid rows" on / off.
    pub fn set_skip_invalid(&mut self, skip: bool, cx: &mut Context<Self>) {
        self.skip_invalid = skip;
        cx.notify();
    }

    /// Maps target column `target` to file field `source` (`None`: not imported, the column
    /// gets its default). A field already mapped elsewhere is moved here.
    pub fn map(&mut self, target: usize, source: Option<usize>, cx: &mut Context<Self>) {
        if target >= self.mapping.len() {
            return;
        }
        if let Some(s) = source {
            for m in &mut self.mapping {
                if *m == Some(s) {
                    *m = None;
                }
            }
        }
        self.mapping[target] = source;
        cx.notify();
    }

    /// Steps the field mapped to `target` (none → field 1 → … → none), for the keyboard.
    pub fn cycle_mapping(&mut self, target: usize, cx: &mut Context<Self>) {
        let fields = self.preview.as_ref().map_or(0, |p| p.field_count);
        let Some(current) = self.mapping.get(target).copied() else {
            return;
        };
        let next = match current {
            None if fields > 0 => Some(0),
            Some(s) if s + 1 < fields => Some(s + 1),
            _ => None,
        };
        self.map(target, next, cx);
    }

    /// The mapping as the core takes it.
    #[must_use]
    pub fn column_mapping(&self) -> ColumnMapping {
        ColumnMapping {
            columns: self
                .mapping
                .iter()
                .enumerate()
                .filter_map(|(target, s)| s.map(|source| MappedColumn { source, target }))
                .collect(),
        }
    }

    /// "Choose…": the desktop's file picker.
    pub fn choose_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let answer = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Import this file".into()),
        });
        self.tasks.push(cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = answer.await
                && let Some(path) = paths.into_iter().next()
            {
                let _ = this.update_in(cx, |d, window, cx| {
                    let text = path.to_string_lossy().into_owned();
                    d.path.update(cx, |s, cx| s.set_value(text, window, cx));
                    d.read_preview(window, cx);
                });
            }
        }));
    }

    /// "Preview": reads the first rows on the core's blocking pool.
    pub fn read_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.path.read(cx).value().trim().to_owned();
        if text.is_empty() {
            self.stage = ImportStage::Failed("Choose the file to import.".into());
            cx.notify();
            return;
        }
        let mut request = CsvPreviewRequest::new(PathBuf::from(text), self.targets.clone());
        request.format = self.format();
        let reply = self.transfer.preview_csv(request);
        self.stage = ImportStage::Previewing;
        cx.notify();
        self.tasks.push(cx.spawn_in(window, async move |this, cx| {
            let result = cx.background_spawn(reply).await;
            let _ = this.update(cx, |d, cx| {
                match result {
                    Some(Ok(preview)) => {
                        d.mapping = vec![None; d.targets.len()];
                        for m in &preview.mapping.columns {
                            if let Some(slot) = d.mapping.get_mut(m.target) {
                                *slot = Some(m.source);
                            }
                        }
                        d.preview = Some(preview);
                        d.stage = ImportStage::Preview;
                    }
                    Some(Err(e)) => {
                        d.stage =
                            ImportStage::Failed(crate::browse::display_message(&e.to_string()));
                    }
                    None => {
                        d.stage = ImportStage::Failed("The preview was interrupted.".into());
                    }
                }
                cx.notify();
            });
        }));
    }

    /// Why "Import" is disabled, if it is.
    #[must_use]
    pub fn start_problem(&self) -> Option<String> {
        if let Some(p) = session_problem(&self.session, true) {
            return Some(p.to_owned());
        }
        if self.preview.is_none() {
            return Some("Read the preview first.".into());
        }
        if let Err(e) = self.column_mapping().validate(&self.targets) {
            return Some(crate::browse::display_message(&e.to_string()));
        }
        if self.transaction == ImportTransaction::AllOrNothing
            && let Some(why) = self.all_or_nothing_refusal()
        {
            return Some(why);
        }
        None
    }

    fn request(&self, cx: &App) -> ImportRequest {
        let path = PathBuf::from(self.path.read(cx).value().trim());
        let request = ImportRequest::new(
            path,
            self.table.clone(),
            self.targets.clone(),
            self.column_mapping(),
        )
        .with_format(self.format())
        .with_transaction(self.transaction)
        .skipping_invalid_rows(self.skip_invalid);
        match &self.table_path {
            Some(p) => request.with_table_path(p.clone()),
            None => request,
        }
    }

    /// "Import": runs, or stops at the core's confirmation.
    pub fn start(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(p) = self.start_problem() {
            self.stage = ImportStage::Failed(p);
            cx.notify();
            return;
        }
        let request = self.request(cx);
        self.run(request, window, cx);
    }

    /// "Import" in the confirmation: runs the same import with the token.
    pub fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ImportStage::Confirm(c) = &self.stage else {
            return;
        };
        let token: ImportToken = c.token;
        let request = self.request(cx).confirmed(token);
        self.run(request, window, cx);
    }

    /// "Cancel" in the confirmation: back to the preview, nothing written.
    pub fn decline(&mut self, cx: &mut Context<Self>) {
        if matches!(self.stage, ImportStage::Confirm(_)) {
            self.stage = ImportStage::Preview;
            cx.notify();
        }
    }

    fn run(&mut self, request: ImportRequest, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(p) = session_problem(&self.session, true) {
            self.stage = ImportStage::Failed(p.to_owned());
            cx.notify();
            return;
        }
        self.ran_with = self.transaction;
        // Never blocks: the core runs the import on the driver runtime.
        let mut job = self.transfer.import_csv(&self.session, request);
        self.stage = ImportStage::Running {
            rows: 0,
            bytes: 0,
            file_bytes: 0,
        };
        let (tx, rx) = unbounded::<ImportEvent>();
        let pump = cx.background_spawn(async move {
            while let Some(event) = job.next().await {
                if tx.send(event).await.is_err() {
                    break;
                }
            }
        });
        let apply = cx.spawn_in(window, async move |this, cx| {
            while let Ok(event) = rx.recv().await {
                if this.update(cx, |d, cx| d.on_event(event, cx)).is_err() {
                    break;
                }
            }
        });
        self.tasks.push(pump);
        self.tasks.push(apply);
        cx.notify();
    }

    fn on_event(&mut self, event: ImportEvent, cx: &mut Context<Self>) {
        if !matches!(self.stage, ImportStage::Running { .. }) {
            return;
        }
        match event {
            ImportEvent::NeedsConfirmation(c) => {
                self.confirm_script = Some(Rc::new(ScriptRows::new(&c.statement)));
                self.stage = ImportStage::Confirm(Box::new(c));
            }
            ImportEvent::Failed(e) => self.stage = ImportStage::Failed(error_text(&e)),
            ImportEvent::Started { file_bytes } => {
                self.stage = ImportStage::Running {
                    rows: 0,
                    bytes: 0,
                    file_bytes,
                };
            }
            ImportEvent::Progress {
                rows_inserted,
                bytes_read,
                file_bytes,
            } => {
                self.stage = ImportStage::Running {
                    rows: rows_inserted,
                    bytes: bytes_read,
                    file_bytes,
                };
            }
            ImportEvent::Done(summary) => self.stage = ImportStage::Done(Box::new(summary)),
            _ => {}
        }
        cx.notify();
    }

    /// "Cancel" while running: drops the job, which cancels it in the core (the transaction is
    /// rolled back; in "commit every N rows" mode the committed rows stay, the summary of a
    /// later run says so).
    pub fn cancel(&mut self, cx: &mut Context<Self>) {
        if matches!(
            self.stage,
            ImportStage::Running { .. } | ImportStage::Previewing
        ) {
            self.tasks.clear();
            let running = matches!(self.stage, ImportStage::Running { .. });
            self.stage = ImportStage::Failed(match self.ran_with {
                _ if !running => "Cancelled: the preview was stopped.".into(),
                ImportTransaction::AllOrNothing => {
                    "Cancelled: the import's transaction is rolled back.".into()
                }
                _ if self
                    .target
                    .as_ref()
                    .is_some_and(|t| !t.rows_can_be_rolled_back()) =>
                {
                    "Cancelled: every row sent so far stays in the table (it cannot roll back)."
                        .into()
                }
                _ => "Cancelled: rows of the batches already committed stay in the table.".into(),
            });
            cx.notify();
        }
    }

    /// The lines of the confirmation, exactly as the core gave them.
    #[must_use]
    pub fn confirmation_lines(c: &ImportConfirmation) -> Vec<(String, String)> {
        let mut lines = vec![
            (
                "Connection".into(),
                format!(
                    "{}{}",
                    crate::browse::display_name(&c.connection_name),
                    if c.production { " (production)" } else { "" }
                ),
            ),
            ("Table".into(), crate::browse::display_name(&c.table)),
            (
                "Columns".into(),
                c.columns
                    .iter()
                    .map(|n| crate::browse::display_name(n))
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
            (
                "File".into(),
                format!(
                    "{} ({})",
                    crate::browse::display_message(&c.file.display().to_string()),
                    bytes_text(c.file_bytes)
                ),
            ),
        ];
        let (commit, when) = match c.transaction() {
            ImportTransaction::AllOrNothing => (
                "All or nothing: one transaction, committed at the end".to_owned(),
                "at the end".to_owned(),
            ),
            ImportTransaction::CommitEvery(n) => (
                format!("A commit every {n} rows: rows committed before an error stay"),
                format!("every {n} rows"),
            ),
            _ => (
                "Committed by the import".to_owned(),
                "by batches".to_owned(),
            ),
        };
        lines.push(("Commits".into(), commit));
        if c.session_manual_commit() {
            lines.push((
                "Note".into(),
                format!("Rows are committed by the import ({when}); Roll back cannot undo them."),
            ));
        }
        if c.non_transactional() {
            lines.push((
                "Note".into(),
                "This table cannot roll back: rows written before a failure stay.".into(),
            ));
        }
        lines
    }

    /// The statement of the confirmation, whole (never cut), as the confirmation shows it.
    #[must_use]
    pub fn confirmation_script(&self) -> Option<Rc<ScriptRows>> {
        matches!(self.stage, ImportStage::Confirm(_))
            .then(|| self.confirm_script.clone())
            .flatten()
    }

    /// The commit mode of the last import started (what its summary describes).
    #[must_use]
    pub fn ran_with(&self) -> ImportTransaction {
        self.ran_with
    }

    /// The summary lines of a finished import run in commit mode `mode`.
    #[must_use]
    pub fn summary_lines(s: &ImportSummary, mode: ImportTransaction) -> Vec<String> {
        let mut lines = SummaryFacts::of(s).lines(mode);
        if s.issues_total > 0 {
            lines.push(format!("{} issue(s):", s.issues_total));
            lines.extend(s.issues.iter().take(50).map(|i| i.describe()));
        }
        lines
    }
}

/// What the summary of an import says, apart from its issues ([`ImportCsvDialog::summary_lines`]).
#[derive(Debug, Clone, PartialEq)]
pub struct SummaryFacts {
    /// How it ended.
    pub end: ImportEnd,
    /// The error's text, if one stopped it.
    pub error: Option<String>,
    /// Data rows read.
    pub rows_read: u64,
    /// Rows the database accepted.
    pub rows_inserted: u64,
    /// Rows committed and kept.
    pub rows_committed: u64,
    /// Invalid rows skipped.
    pub rows_skipped: u64,
    /// Rows not committed could be rolled back: the import ran in transactions on a table
    /// that rolls back ([`ImportSummary::transactional`] and [`ImportSummary::target`]).
    pub transactional: bool,
    /// Wall-clock seconds.
    pub elapsed_secs: f64,
}

impl SummaryFacts {
    /// The facts of a core summary.
    #[must_use]
    pub fn of(s: &ImportSummary) -> Self {
        Self {
            end: s.end,
            error: s.error.as_ref().map(error_text),
            rows_read: s.rows_read,
            rows_inserted: s.rows_inserted,
            rows_committed: s.rows_committed,
            rows_skipped: s.rows_skipped,
            // A table that cannot roll back (or an engine without transactions) keeps every
            // row it accepted: never worded as rolled back.
            transactional: s.transactional && s.target.rows_can_be_rolled_back(),
            elapsed_secs: s.elapsed.as_secs_f64(),
        }
    }

    /// The lines, worded from the commit mode the user chose (`mode`): what was committed is
    /// said first, so a "commit every N rows" import that stopped never reads as rolled back
    /// entirely. Matches docs/tutorials/export-import.md §5 step 7.
    #[must_use]
    pub fn lines(&self, mode: ImportTransaction) -> Vec<String> {
        let end = match self.end {
            ImportEnd::Completed => "Import completed",
            ImportEnd::Cancelled => "Import cancelled",
            ImportEnd::Failed => "Import failed",
            _ => "Import ended",
        };
        let mut lines = vec![format!(
            "{end}: {} rows read, {} inserted, {} committed, {} skipped, in {:.1} s.",
            self.rows_read,
            self.rows_inserted,
            self.rows_committed,
            self.rows_skipped,
            self.elapsed_secs
        )];
        if let Some(e) = &self.error {
            lines.push(e.clone());
        }
        if self.end == ImportEnd::Completed {
            return lines;
        }
        // Rows inserted after the last commit, rolled back with the open batch.
        let uncommitted = self.rows_inserted.saturating_sub(self.rows_committed);
        if self.rows_committed > 0 {
            lines.push(format!(
                "{} rows were committed before the end and stay in the table.",
                self.rows_committed
            ));
            if self.transactional && uncommitted > 0 {
                lines.push(format!(
                    "The {uncommitted} rows inserted after the last commit were rolled back."
                ));
            }
        } else {
            lines.push(match mode {
                ImportTransaction::AllOrNothing if self.transactional => {
                    "The transaction was rolled back: nothing was written.".into()
                }
                _ => "No rows were committed before the end: nothing was written.".into(),
            });
        }
        lines
    }
}

impl Render for ImportCsvDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let running = matches!(
            self.stage,
            ImportStage::Running { .. } | ImportStage::Previewing
        );
        let table = crate::browse::display_name(&self.table.join("."));
        let refusal = self.all_or_nothing_refusal();
        let mut body = v_flex()
            .gap_2()
            .text_sm()
            .child(format!("Rows of a CSV file are inserted into {table}."))
            .child(
                h_flex()
                    .gap_2()
                    .child(div().flex_1().child(Input::new(&self.path)))
                    .child(
                        Button::new("import-choose")
                            .xsmall()
                            .label("Choose…")
                            .disabled(running)
                            .on_click(cx.listener(|d, _, window, cx| d.choose_file(window, cx))),
                    )
                    .child(
                        Button::new("import-preview")
                            .xsmall()
                            .label("Preview")
                            .disabled(running)
                            .on_click(cx.listener(|d, _, window, cx| d.read_preview(window, cx))),
                    )
                    .child(
                        Button::new("import-header")
                            .xsmall()
                            .label(if self.header {
                                "Header row: yes"
                            } else {
                                "Header row: no"
                            })
                            .disabled(running)
                            .on_click(cx.listener(|d, _, window, cx| {
                                let v = !d.header;
                                d.set_header(v, window, cx);
                            })),
                    ),
            );
        if let Some(p) = &self.preview {
            let field_name = |i: usize| -> String {
                p.header.as_ref().and_then(|h| h.get(i)).map_or_else(
                    || format!("field {}", i + 1),
                    |n| crate::browse::display_name(n),
                )
            };
            let rows = p.rows.iter().take(SHOWN_PREVIEW_ROWS).map(|r| {
                let cells = r
                    .fields
                    .iter()
                    .map(|f| ninesql_grid::format::display_line(f, 40).0)
                    .collect::<Vec<_>>()
                    .join(" | ");
                div()
                    .font_family("monospace")
                    .text_xs()
                    .child(format!("{:>5}  {cells}", r.line))
            });
            let mapping = self.targets.iter().enumerate().map(|(t, col)| {
                let label = match self.mapping.get(t).copied().flatten() {
                    Some(s) => format!("← {}", field_name(s)),
                    None => "← (not imported: default)".to_owned(),
                };
                h_flex()
                    .gap_2()
                    .child(div().w(px(200.)).child(format!(
                        "{} ({})",
                        crate::browse::display_name(&col.name),
                        col.target_type().as_str()
                    )))
                    .child(
                        Button::new(("import-map", t))
                            .xsmall()
                            .label(label)
                            .tooltip("Change the file column (cycles)")
                            .disabled(running)
                            .on_click(cx.listener(move |d, _, _, cx| d.cycle_mapping(t, cx))),
                    )
            });
            body = body
                .child(div().font_weight(FontWeight::BOLD).child(format!(
                    "Preview: {} field(s){}",
                    p.field_count,
                    if p.complete { ", whole file read" } else { "" }
                )))
                .children(rows)
                .when(p.issues_total > 0, |d| {
                    d.child(div().text_color(theme.warning).child(format!(
                        "{} issue(s) in the previewed rows:",
                        p.issues_total
                    )))
                    .children(p.issues.iter().take(10).map(|i| {
                        div()
                            .text_xs()
                            .text_color(theme.warning)
                            .child(i.describe())
                    }))
                })
                .child(div().font_weight(FontWeight::BOLD).child("Column mapping"))
                .children(mapping)
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            Button::new("import-transaction")
                                .xsmall()
                                .label(match self.transaction {
                                    ImportTransaction::AllOrNothing => {
                                        "All or nothing (one transaction)".to_owned()
                                    }
                                    ImportTransaction::CommitEvery(n) => {
                                        format!("Commit every {n} rows")
                                    }
                                    _ => "Transaction mode".to_owned(),
                                })
                                // Only "commit every N rows" when the table cannot roll back.
                                .disabled(running || refusal.is_some())
                                .when_some(refusal.clone(), |b, why| b.tooltip(why))
                                .on_click(cx.listener(|d, _, _, cx| {
                                    let next = match d.transaction {
                                        ImportTransaction::AllOrNothing => {
                                            ImportTransaction::CommitEvery(COMMIT_EVERY_ROWS)
                                        }
                                        _ => ImportTransaction::AllOrNothing,
                                    };
                                    d.set_transaction(next, cx);
                                })),
                        )
                        .child(
                            Button::new("import-skip")
                                .xsmall()
                                .label(if self.skip_invalid {
                                    "Invalid rows: skip"
                                } else {
                                    "Invalid rows: stop"
                                })
                                .disabled(running)
                                .on_click(cx.listener(|d, _, _, cx| {
                                    let v = !d.skip_invalid;
                                    d.set_skip_invalid(v, cx);
                                })),
                        ),
                )
                .when_some(refusal, |d, why| {
                    d.child(
                        div()
                            .text_xs()
                            .text_color(theme.warning)
                            .child(format!("\"All or nothing\" is not offered: {why}")),
                    )
                })
                .when_some(self.target_error.clone(), |d, why| {
                    d.child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(format!("The table's storage could not be checked: {why}")),
                    )
                });
        }
        body = match &self.stage {
            ImportStage::ChooseFile | ImportStage::Preview => body,
            ImportStage::Previewing => body.child("Reading the preview…"),
            ImportStage::Confirm(c) => body.child(
                v_flex()
                    .id("import-confirm")
                    .gap_1()
                    .p_2()
                    .border_2()
                    .border_color(if c.production {
                        theme.danger
                    } else {
                        theme.warning
                    })
                    .child(
                        div()
                            .font_weight(FontWeight::BOLD)
                            .child("Confirm the import: these rows are written to the database."),
                    )
                    .children(Self::confirmation_lines(c).into_iter().map(|(k, v)| {
                        h_flex()
                            .gap_2()
                            .items_start()
                            .child(div().w(px(90.)).text_color(theme.muted_foreground).child(k))
                            .child(div().flex_1().font_family("monospace").text_xs().child(v))
                    }))
                    .when_some(self.confirmation_script(), |d, script| {
                        d.child(div().text_color(theme.muted_foreground).child("Statement"))
                            .child(script_view("import-statement", script, cx))
                    }),
            ),
            ImportStage::Running {
                rows,
                bytes,
                file_bytes,
            } => body.child(format!(
                "Importing… {rows} rows inserted, {} of {} read",
                bytes_text(*bytes),
                bytes_text(*file_bytes)
            )),
            ImportStage::Done(s) => body.children(
                Self::summary_lines(s, self.ran_with)
                    .into_iter()
                    .map(|l| div().child(l)),
            ),
            ImportStage::Failed(why) => {
                body.child(div().text_color(theme.danger).child(why.clone()))
            }
        };
        let problem = self.start_problem();
        let footer = match &self.stage {
            ImportStage::Confirm(_) => h_flex()
                .gap_2()
                .justify_end()
                .child(
                    Button::new("import-decline")
                        .label("Cancel")
                        .on_click(cx.listener(|d, _, _, cx| d.decline(cx))),
                )
                .child(
                    Button::new("import-confirm-run")
                        .danger()
                        .label("Import")
                        .on_click(cx.listener(|d, _, window, cx| d.confirm(window, cx))),
                ),
            _ if running => h_flex().gap_2().justify_end().child(
                Button::new("import-cancel")
                    .danger()
                    .label("Cancel")
                    .on_click(cx.listener(|d, _, _, cx| d.cancel(cx))),
            ),
            _ => h_flex()
                .gap_2()
                .justify_end()
                .child(
                    Button::new("import-close")
                        .label("Close")
                        .on_click(|_, window, cx| window.close_dialog(cx)),
                )
                .child(
                    Button::new("import-start")
                        .primary()
                        .label("Import")
                        .tooltip(SharedString::from(
                            problem.clone().unwrap_or_else(|| "Import the file".into()),
                        ))
                        .disabled(problem.is_some())
                        .on_click(cx.listener(|d, _, window, cx| d.start(window, cx))),
                ),
        };
        v_flex()
            .id("import-csv")
            .gap_3()
            .max_h(px(640.))
            .overflow_y_scroll()
            .child(body)
            .child(footer)
    }
}

/// Opens the import dialog in `window`.
pub fn open_import_dialog(
    session: SessionHandle,
    transfer: TransferService,
    table: Vec<String>,
    table_path: Option<ObjectPath>,
    targets: Vec<TargetColumn>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<ImportCsvDialog> {
    let dialog = cx
        .new(|cx| ImportCsvDialog::new(session, transfer, table, table_path, targets, window, cx));
    let view = dialog.clone();
    window.open_dialog(cx, move |d, _, _| {
        d.title("Import CSV").w(px(720.)).child(view.clone())
    });
    #[cfg(test)]
    super::OPENED_IMPORT.with(|o| *o.borrow_mut() = Some(dialog.clone()));
    dialog
}

/// The target columns of a table from its listed column objects, in table order.
#[must_use]
pub fn targets_from(items: &[ninesql_core::driver_api::metadata::ObjectInfo]) -> Vec<TargetColumn> {
    use ninesql_core::driver_api::metadata::ObjectDetails;
    let mut cols: Vec<(u32, TargetColumn)> = items
        .iter()
        .filter_map(|i| match &i.details {
            ObjectDetails::Column(c) => Some((c.ordinal, TargetColumn::from_info(i.name(), c))),
            _ => None,
        })
        .collect();
    cols.sort_by_key(|(o, _)| *o);
    cols.into_iter().map(|(_, c)| c).collect()
}

#[cfg(test)]
mod summary_tests {
    use ninesql_core::{ImportEnd, ImportTransaction};

    use super::{COMMIT_EVERY_ROWS, SummaryFacts};

    fn facts(end: ImportEnd, inserted: u64, committed: u64) -> SummaryFacts {
        SummaryFacts {
            end,
            error: (end == ImportEnd::Failed).then(|| "line 5501: not a number".into()),
            rows_read: inserted + u64::from(end != ImportEnd::Completed),
            rows_inserted: inserted,
            rows_committed: committed,
            rows_skipped: 0,
            // PostgreSQL, MySQL and SQLite: transactional whatever the mode.
            transactional: true,
            elapsed_secs: 1.2,
        }
    }

    const EVERY: ImportTransaction = ImportTransaction::CommitEvery(COMMIT_EVERY_ROWS);
    const ROLLED_BACK: &str = "The transaction was rolled back: nothing was written.";

    #[test]
    fn success_says_only_the_counts_in_both_modes() {
        for mode in [ImportTransaction::AllOrNothing, EVERY] {
            assert_eq!(
                facts(ImportEnd::Completed, 7000, 7000).lines(mode),
                vec![
                    "Import completed: 7000 rows read, 7000 inserted, 7000 committed, 0 skipped, in 1.2 s."
                ]
            );
        }
    }

    #[test]
    fn commit_every_failure_after_commits_never_says_rolled_back_entirely() {
        let lines = facts(ImportEnd::Failed, 5500, 5000).lines(EVERY);
        assert_eq!(
            lines,
            vec![
                "Import failed: 5501 rows read, 5500 inserted, 5000 committed, 0 skipped, in 1.2 s.",
                "line 5501: not a number",
                "5000 rows were committed before the end and stay in the table.",
                "The 500 rows inserted after the last commit were rolled back.",
            ]
        );
        assert!(!lines.iter().any(|l| l == ROLLED_BACK));
        // Cancelled the same way.
        let lines = facts(ImportEnd::Cancelled, 2000, 2000).lines(EVERY);
        assert_eq!(
            lines.last().map(String::as_str),
            Some("2000 rows were committed before the end and stay in the table.")
        );
    }

    #[test]
    fn commit_every_failure_before_any_commit() {
        let lines = facts(ImportEnd::Failed, 600, 0).lines(EVERY);
        assert_eq!(
            lines.last().map(String::as_str),
            Some("No rows were committed before the end: nothing was written.")
        );
        assert!(!lines.iter().any(|l| l == ROLLED_BACK));
    }

    #[test]
    fn all_or_nothing_failure_rolls_everything_back() {
        let lines = facts(ImportEnd::Failed, 5500, 0).lines(ImportTransaction::AllOrNothing);
        assert_eq!(lines.last().map(String::as_str), Some(ROLLED_BACK));
        assert_eq!(lines.len(), 3);
        let lines = facts(ImportEnd::Cancelled, 10, 0).lines(ImportTransaction::AllOrNothing);
        assert_eq!(
            lines,
            vec![
                "Import cancelled: 11 rows read, 10 inserted, 0 committed, 0 skipped, in 1.2 s."
                    .to_owned(),
                ROLLED_BACK.to_owned(),
            ]
        );
    }

    #[test]
    fn non_transactional_table_is_never_said_rolled_back() {
        let mut f = facts(ImportEnd::Failed, 0, 0);
        f.transactional = false;
        for mode in [ImportTransaction::AllOrNothing, EVERY] {
            let lines = f.lines(mode);
            assert!(
                !lines.iter().any(|l| l.contains("rolled back")),
                "{lines:?}"
            );
        }
        let mut f = facts(ImportEnd::Cancelled, 1500, 1500);
        f.transactional = false;
        let lines = f.lines(EVERY);
        assert!(
            !lines.iter().any(|l| l.contains("rolled back")),
            "{lines:?}"
        );
    }

    #[test]
    fn non_transactional_engine_keeps_every_inserted_row() {
        let mut f = facts(ImportEnd::Failed, 300, 300);
        f.transactional = false;
        let lines = f.lines(EVERY);
        assert_eq!(
            lines.last().map(String::as_str),
            Some("300 rows were committed before the end and stay in the table.")
        );
    }
}
