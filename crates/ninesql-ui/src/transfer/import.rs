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
use ninesql_core::transfer::{ColumnMapping, CsvFormat, CsvPreview, MappedColumn, TargetColumn};
use ninesql_core::{
    CsvPreviewRequest, ImportConfirmation, ImportEnd, ImportEvent, ImportRequest, ImportSummary,
    ImportToken, ImportTransaction, SessionHandle, TransferService,
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
    targets: Vec<TargetColumn>,
    path: Entity<InputState>,
    header: bool,
    preview: Option<CsvPreview>,
    /// `mapping[target] = Some(file field)`.
    mapping: Vec<Option<usize>>,
    transaction: ImportTransaction,
    skip_invalid: bool,
    stage: ImportStage,
    /// The statement of the confirmation, laid out once ([`ScriptRows`]).
    confirm_script: Option<Rc<ScriptRows>>,
    tasks: Vec<Task<()>>,
}

impl ImportCsvDialog {
    /// A dialog importing into `table` (its `targets` columns, from the metadata cache).
    pub fn new(
        session: SessionHandle,
        transfer: TransferService,
        table: Vec<String>,
        targets: Vec<TargetColumn>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let path = cx.new(|cx| InputState::new(window, cx).placeholder("/home/me/data.csv"));
        let mapping = vec![None; targets.len()];
        Self {
            session,
            transfer,
            table,
            targets,
            path,
            header: true,
            preview: None,
            mapping,
            transaction: ImportTransaction::AllOrNothing,
            skip_invalid: false,
            stage: ImportStage::ChooseFile,
            confirm_script: None,
            tasks: Vec::new(),
        }
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

    /// "All or nothing" (one transaction) or "commit every N rows".
    pub fn set_transaction(&mut self, t: ImportTransaction, cx: &mut Context<Self>) {
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
        None
    }

    fn request(&self, cx: &App) -> ImportRequest {
        let path = PathBuf::from(self.path.read(cx).value().trim());
        ImportRequest::new(
            path,
            self.table.clone(),
            self.targets.clone(),
            self.column_mapping(),
        )
        .with_format(self.format())
        .with_transaction(self.transaction)
        .skipping_invalid_rows(self.skip_invalid)
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
            self.stage = ImportStage::Failed(match self.transaction {
                ImportTransaction::AllOrNothing => {
                    "Cancelled: the import's transaction is rolled back.".into()
                }
                _ => "Cancelled: rows of the batches already committed stay in the table.".into(),
            });
            cx.notify();
        }
    }

    /// The lines of the confirmation, exactly as the core gave them.
    #[must_use]
    pub fn confirmation_lines(c: &ImportConfirmation) -> Vec<(String, String)> {
        vec![
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
        ]
    }

    /// The statement of the confirmation, whole (never cut), as the confirmation shows it.
    #[must_use]
    pub fn confirmation_script(&self) -> Option<Rc<ScriptRows>> {
        matches!(self.stage, ImportStage::Confirm(_))
            .then(|| self.confirm_script.clone())
            .flatten()
    }

    /// The summary lines of a finished import.
    #[must_use]
    pub fn summary_lines(s: &ImportSummary) -> Vec<String> {
        let end = match s.end {
            ImportEnd::Completed => "Import completed",
            ImportEnd::Cancelled => "Import cancelled",
            ImportEnd::Failed => "Import failed",
            _ => "Import ended",
        };
        let mut lines = vec![format!(
            "{end}: {} rows read, {} inserted, {} committed, {} skipped, in {:.1} s.",
            s.rows_read,
            s.rows_inserted,
            s.rows_committed,
            s.rows_skipped,
            s.elapsed.as_secs_f64()
        )];
        if let Some(e) = &s.error {
            lines.push(error_text(e));
        }
        if s.end != ImportEnd::Completed && s.transactional {
            lines.push("The transaction was rolled back: nothing was written.".into());
        } else if s.end != ImportEnd::Completed && s.rows_committed > 0 {
            lines.push(format!(
                "{} rows were committed before the end and stay in the table.",
                s.rows_committed
            ));
        }
        if s.issues_total > 0 {
            lines.push(format!("{} issue(s):", s.issues_total));
            lines.extend(s.issues.iter().take(50).map(|i| i.describe()));
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
                                .disabled(running)
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
                );
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
            ImportStage::Done(s) => {
                body.children(Self::summary_lines(s).into_iter().map(|l| div().child(l)))
            }
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
    targets: Vec<TargetColumn>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<ImportCsvDialog> {
    let dialog = cx.new(|cx| ImportCsvDialog::new(session, transfer, table, targets, window, cx));
    let view = dialog.clone();
    window.open_dialog(cx, move |d, _, _| {
        d.title("Import CSV").w(px(720.)).child(view.clone())
    });
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
