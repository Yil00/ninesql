//! "Export results…" and "Export table…" (F-XFER-01): format and options, a new target file,
//! an explicit overwrite step, progress, cancel and a summary with the warnings.

use std::path::PathBuf;

use gpui_kit::base::async_util::unbounded;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use ninesql_core::transfer::{ExportFormat, ExportOptions, Overwrite, TransferError};
use ninesql_core::{
    ExportEvent, ExportRequest, ExportSummary, ExportWarning, SessionHandle, TransferService,
};

use super::{bytes_text, error_text, session_problem};

/// What is exported.
#[derive(Clone, PartialEq, Eq)]
pub enum ExportTarget {
    /// The statement of a result, run again (a read only; the core checks it).
    Statement {
        /// Statement text.
        text: String,
    },
    /// Every row of a table (`SELECT * FROM <table>`, quoted by the core).
    Table {
        /// Name parts.
        parts: Vec<String>,
    },
}

impl std::fmt::Debug for ExportTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Statement { text } => write!(f, "Statement({} bytes)", text.len()),
            Self::Table { parts } => write!(f, "Table({} parts)", parts.len()),
        }
    }
}

/// Where the dialog stands.
#[derive(Debug)]
pub enum ExportStage {
    /// Choosing the format, the options and the file.
    Editing,
    /// Checking the target file.
    Checking,
    /// The file exists: replacing it needs this explicit step.
    ConfirmOverwrite(PathBuf),
    /// Running.
    Running {
        /// Rows written.
        rows: u64,
        /// Bytes written.
        bytes: u64,
    },
    /// Finished.
    Done(ExportSummary),
    /// Failed or refused (message without values).
    Failed(String),
    /// Cancelled by the user (the partial file is removed by the core).
    Cancelled,
}

/// Every format offered, in menu order.
pub const FORMATS: [ExportFormat; 7] = [
    ExportFormat::Csv,
    ExportFormat::Tsv,
    ExportFormat::JsonArray,
    ExportFormat::JsonLines,
    ExportFormat::SqlInsert,
    ExportFormat::Markdown,
    ExportFormat::Html,
];

/// Menu label of a format.
#[must_use]
pub fn format_label(format: ExportFormat) -> &'static str {
    match format {
        ExportFormat::Csv => "CSV",
        ExportFormat::Tsv => "TSV",
        ExportFormat::JsonArray => "JSON",
        ExportFormat::JsonLines => "JSON Lines",
        ExportFormat::SqlInsert => "SQL INSERT",
        ExportFormat::Markdown => "Markdown",
        ExportFormat::Html => "HTML",
        _ => "Other",
    }
}

/// User-facing text of an export warning.
#[must_use]
pub fn warning_text(w: ExportWarning) -> &'static str {
    match w {
        ExportWarning::TransactionAlreadyOpen => {
            "A transaction was open on the session: the export read inside it (uncommitted changes included)."
        }
        ExportWarning::ReadOnlyTransactionFailed => {
            "The export could not open a read-only transaction: it ran without one."
        }
        ExportWarning::NoReadOnlyTransaction => {
            "This engine (TiDB) has no engine-side read-only transaction: the export ran without one, protected only by NineSQL's check that the statement reads. Export from a read-only connection to have the engine enforce it."
        }
        _ => "The export reported a warning.",
    }
}

/// The export dialog.
pub struct ExportDialog {
    session: SessionHandle,
    transfer: TransferService,
    target: ExportTarget,
    format: ExportFormat,
    header: bool,
    formula_guard: bool,
    path: Entity<InputState>,
    stage: ExportStage,
    /// The job's tasks: dropping them drops the job, which cancels it in the core.
    tasks: Vec<Task<()>>,
}

impl ExportDialog {
    /// A dialog exporting `target` on `session`.
    pub fn new(
        session: SessionHandle,
        transfer: TransferService,
        target: ExportTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let path = cx
            .new(|cx| InputState::new(window, cx).placeholder("/home/me/export.csv (a new file)"));
        Self {
            session,
            transfer,
            target,
            format: ExportFormat::Csv,
            header: true,
            formula_guard: true,
            path,
            stage: ExportStage::Editing,
            tasks: Vec::new(),
        }
    }

    /// The stage.
    #[must_use]
    pub fn stage(&self) -> &ExportStage {
        &self.stage
    }

    /// The target.
    #[must_use]
    pub fn target(&self) -> &ExportTarget {
        &self.target
    }

    /// The format.
    #[must_use]
    pub fn format(&self) -> ExportFormat {
        self.format
    }

    /// The path input.
    #[must_use]
    pub fn path_input(&self) -> &Entity<InputState> {
        &self.path
    }

    /// Chooses the format (and keeps the file name's extension in step).
    pub fn set_format(
        &mut self,
        format: ExportFormat,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.format = format;
        let current = self.path.read(cx).value().to_string();
        if !current.is_empty() {
            let mut p = PathBuf::from(current);
            p.set_extension(format.extension());
            let text = p.to_string_lossy().into_owned();
            self.path.update(cx, |s, cx| s.set_value(text, window, cx));
        }
        cx.notify();
    }

    /// Header row on / off (CSV, TSV).
    pub fn set_header(&mut self, header: bool, cx: &mut Context<Self>) {
        self.header = header;
        cx.notify();
    }

    /// Spreadsheet formula guard on / off (CSV, TSV).
    pub fn set_formula_guard(&mut self, on: bool, cx: &mut Context<Self>) {
        self.formula_guard = on;
        cx.notify();
    }

    /// Whether a job is running or the file is being checked.
    #[must_use]
    pub fn is_busy(&self) -> bool {
        matches!(
            self.stage,
            ExportStage::Checking | ExportStage::Running { .. }
        )
    }

    /// Why "Export" is disabled, if it is.
    #[must_use]
    pub fn start_problem(&self, cx: &App) -> Option<&'static str> {
        if let Some(p) = session_problem(&self.session, false) {
            return Some(p);
        }
        if self.path.read(cx).value().trim().is_empty() {
            return Some("Choose the file to write.");
        }
        None
    }

    fn options(&self) -> ExportOptions {
        let mut o = ExportOptions::new(self.format);
        if matches!(self.format, ExportFormat::Csv | ExportFormat::Tsv) {
            o.header = self.header;
            o.formula_guard = self.formula_guard;
        }
        o
    }

    /// "Choose…": the desktop's save dialog (XDG portal on Linux).
    pub fn choose_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dir = std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from);
        let name = format!("export.{}", self.format.extension());
        let answer = cx.prompt_for_new_path(&dir, Some(&name));
        self.tasks.push(cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(path))) = answer.await {
                let _ = this.update_in(cx, |d, window, cx| {
                    let text = path.to_string_lossy().into_owned();
                    d.path.update(cx, |s, cx| s.set_value(text, window, cx));
                    // The save dialog asked about replacing: still our own explicit step.
                    cx.notify();
                });
            }
        }));
    }

    /// "Export": checks the file (off the UI thread), then runs, or asks about overwriting.
    pub fn start(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_busy() {
            return;
        }
        if let Some(p) = self.start_problem(cx) {
            self.stage = ExportStage::Failed(p.to_owned());
            cx.notify();
            return;
        }
        let path = PathBuf::from(self.path.read(cx).value().trim());
        self.check(path, Overwrite::Refuse, window, cx);
    }

    /// "Replace": the user confirmed overwriting the existing file.
    pub fn confirm_overwrite(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ExportStage::ConfirmOverwrite(path) = &self.stage else {
            return;
        };
        let path = path.clone();
        self.check(path, Overwrite::Confirmed, window, cx);
    }

    /// Back to editing (from the overwrite step or a summary).
    pub fn back(&mut self, cx: &mut Context<Self>) {
        if !self.is_busy() {
            self.stage = ExportStage::Editing;
            cx.notify();
        }
    }

    fn check(
        &mut self,
        path: PathBuf,
        overwrite: Overwrite,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.stage = ExportStage::Checking;
        cx.notify();
        let service = self.transfer.clone();
        let checked = cx.background_spawn(async move {
            let result = service.check_export_path(&path, overwrite);
            (path, result)
        });
        self.tasks.push(cx.spawn_in(window, async move |this, cx| {
            let (path, result) = checked.await;
            let _ = this.update_in(cx, |d, window, cx| match result {
                Ok(target) => d.run(target, overwrite, window, cx),
                Err(TransferError::AlreadyExists(_)) if overwrite == Overwrite::Refuse => {
                    d.stage = ExportStage::ConfirmOverwrite(path);
                    cx.notify();
                }
                Err(e) => {
                    d.stage = ExportStage::Failed(crate::browse::display_message(&e.to_string()));
                    cx.notify();
                }
            });
        }));
    }

    fn run(
        &mut self,
        target: PathBuf,
        overwrite: Overwrite,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(p) = session_problem(&self.session, false) {
            self.stage = ExportStage::Failed(p.to_owned());
            cx.notify();
            return;
        }
        let options = self.options();
        let mut request = match &self.target {
            ExportTarget::Statement { text } => {
                ExportRequest::statement(text.clone(), target, options)
            }
            ExportTarget::Table { parts } => ExportRequest::table(parts.clone(), target, options),
        };
        if overwrite == Overwrite::Confirmed {
            request = request.overwrite_confirmed();
        }
        // Never blocks: the core runs the export on the driver runtime.
        let mut job = self.transfer.export(&self.session, request);
        self.stage = ExportStage::Running { rows: 0, bytes: 0 };
        let (tx, rx) = unbounded::<ExportEvent>();
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

    fn on_event(&mut self, event: ExportEvent, cx: &mut Context<Self>) {
        if !matches!(self.stage, ExportStage::Running { .. }) {
            return;
        }
        match event {
            ExportEvent::Started => {}
            ExportEvent::Progress { rows, bytes } => {
                self.stage = ExportStage::Running { rows, bytes };
            }
            ExportEvent::Done(summary) => self.stage = ExportStage::Done(summary),
            ExportEvent::Failed(e) => self.stage = ExportStage::Failed(error_text(&e)),
            _ => {}
        }
        cx.notify();
    }

    /// "Cancel" while running: drops the job, which cancels it in the core (the partial file is
    /// removed).
    pub fn cancel(&mut self, cx: &mut Context<Self>) {
        if matches!(
            self.stage,
            ExportStage::Running { .. } | ExportStage::Checking
        ) {
            self.tasks.clear();
            self.stage = ExportStage::Cancelled;
            cx.notify();
        }
    }

    /// The summary lines of a finished export.
    #[must_use]
    pub fn summary_lines(summary: &ExportSummary) -> Vec<String> {
        let mut lines = vec![format!(
            "{} rows, {} written to {} in {:.1} s.",
            summary.rows,
            bytes_text(summary.bytes),
            summary.path.display(),
            summary.elapsed.as_secs_f64()
        )];
        if summary.incomplete {
            lines.push(
                "Incomplete: the driver stopped the result at its budget; the file holds the rows delivered before."
                    .into(),
            );
        }
        if !summary.truncated_value_columns.is_empty() {
            lines.push(format!(
                "Values cut at the value size limit in: {}.",
                summary
                    .truncated_value_columns
                    .iter()
                    .map(|c| crate::browse::display_name(c))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if summary.result_sets_skipped > 0 {
            lines.push(format!(
                "{} further result set(s) were not exported.",
                summary.result_sets_skipped
            ));
        }
        lines.extend(summary.warnings.iter().map(|w| warning_text(*w).to_owned()));
        lines
    }
}

impl Render for ExportDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let busy = self.is_busy();
        let editing = matches!(
            self.stage,
            ExportStage::Editing | ExportStage::Failed(_) | ExportStage::Cancelled
        );
        let what = match &self.target {
            ExportTarget::Statement { .. } => {
                "The statement of the result runs again (reads only); rows are streamed to the file.".to_owned()
            }
            ExportTarget::Table { parts } => format!(
                "Every row of {} is streamed to the file.",
                crate::browse::display_name(&parts.join("."))
            ),
        };
        let formats = FORMATS.iter().map(|f| {
            let f = *f;
            Button::new(SharedString::from(format!(
                "export-format-{}",
                f.extension()
            )))
            .xsmall()
            .label(format_label(f))
            .when(f == self.format, |b| b.primary())
            .disabled(!editing)
            .on_click(cx.listener(move |d, _, window, cx| d.set_format(f, window, cx)))
        });
        let csv_like = matches!(self.format, ExportFormat::Csv | ExportFormat::Tsv);
        let problem = self.start_problem(cx);
        let mut body = v_flex()
            .gap_2()
            .text_sm()
            .child(what)
            .child(h_flex().gap_1().flex_wrap().children(formats))
            .when(csv_like, |d| {
                d.child(
                    h_flex()
                        .gap_2()
                        .child(
                            Button::new("export-header")
                                .xsmall()
                                .label(if self.header {
                                    "Header row: on"
                                } else {
                                    "Header row: off"
                                })
                                .disabled(!editing)
                                .on_click(cx.listener(|d, _, _, cx| {
                                    let v = !d.header;
                                    d.set_header(v, cx);
                                })),
                        )
                        .child(
                            Button::new("export-guard")
                                .xsmall()
                                .label(if self.formula_guard {
                                    "Formula guard: on"
                                } else {
                                    "Formula guard: off"
                                })
                                .tooltip(
                                    "Prefix cells that a spreadsheet would run as formulas with '",
                                )
                                .disabled(!editing)
                                .on_click(cx.listener(|d, _, _, cx| {
                                    let v = !d.formula_guard;
                                    d.set_formula_guard(v, cx);
                                })),
                        ),
                )
            })
            .child(
                h_flex()
                    .gap_2()
                    .child(div().flex_1().child(Input::new(&self.path)))
                    .child(
                        Button::new("export-choose")
                            .xsmall()
                            .label("Choose…")
                            .disabled(!editing)
                            .on_click(cx.listener(|d, _, window, cx| d.choose_file(window, cx))),
                    ),
            );
        body = match &self.stage {
            ExportStage::Editing => body,
            ExportStage::Checking => body.child("Checking the file…"),
            ExportStage::ConfirmOverwrite(path) => body.child(
                div()
                    .id("export-overwrite")
                    .text_color(theme.warning)
                    .child(format!(
                        "{} already exists. Replace it? Its current content is lost.",
                        path.display()
                    )),
            ),
            ExportStage::Running { rows, bytes } => {
                body.child(format!("Exporting… {rows} rows, {}", bytes_text(*bytes)))
            }
            ExportStage::Done(summary) => body.children(
                Self::summary_lines(summary)
                    .into_iter()
                    .map(|l| div().child(l)),
            ),
            ExportStage::Failed(why) => {
                body.child(div().text_color(theme.danger).child(why.clone()))
            }
            ExportStage::Cancelled => body.child("Cancelled: the partial file was removed."),
        };
        let mut footer = h_flex().gap_2().justify_end();
        footer = match &self.stage {
            ExportStage::ConfirmOverwrite(_) => footer
                .child(
                    Button::new("export-back")
                        .label("Back")
                        .on_click(cx.listener(|d, _, _, cx| d.back(cx))),
                )
                .child(
                    Button::new("export-replace")
                        .danger()
                        .label("Replace")
                        .on_click(cx.listener(|d, _, window, cx| d.confirm_overwrite(window, cx))),
                ),
            _ if busy => footer.child(
                Button::new("export-cancel")
                    .danger()
                    .label("Cancel")
                    .on_click(cx.listener(|d, _, _, cx| d.cancel(cx))),
            ),
            _ => footer
                .child(
                    Button::new("export-close")
                        .label("Close")
                        .on_click(|_, window, cx| window.close_dialog(cx)),
                )
                .child(
                    Button::new("export-start")
                        .primary()
                        .label("Export")
                        .tooltip(problem.unwrap_or("Export to the file"))
                        .disabled(problem.is_some())
                        .on_click(cx.listener(|d, _, window, cx| d.start(window, cx))),
                ),
        };
        v_flex().gap_3().child(body).child(footer)
    }
}

/// Opens the export dialog in `window`.
pub fn open_export_dialog(
    session: SessionHandle,
    transfer: TransferService,
    target: ExportTarget,
    window: &mut Window,
    cx: &mut App,
) -> Entity<ExportDialog> {
    let title = match &target {
        ExportTarget::Statement { .. } => "Export results",
        ExportTarget::Table { .. } => "Export table",
    };
    let dialog = cx.new(|cx| ExportDialog::new(session, transfer, target, window, cx));
    let view = dialog.clone();
    window.open_dialog(cx, move |d, _, _| {
        d.title(title).w(px(620.)).child(view.clone())
    });
    dialog
}
