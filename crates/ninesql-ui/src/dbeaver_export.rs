//! "Export connections to DBeaver…" (P5-D, F-CON-15): choose connections, then a **new** file
//! (`data-sources.json`) or a **new** DBeaver project folder; the core writes it on its
//! blocking pool (`CoreServices::export_dbeaver`, I6), never a secret (I1), never inside a
//! DBeaver location (I4). The report says what was written, skipped, dropped, approximated or
//! not verified. When the core answers `DbeaverExportError::Changed` (the target changed while
//! it was written), nothing was written and the dialog asks for the target again.

use std::collections::BTreeSet;
use std::path::PathBuf;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use ninesql_core::dbeaver::export::{ExportIssue, ExportIssueKind, ExportTarget};
use ninesql_core::{ConnectionId, CoreServices, DbeaverExportError, DbeaverExportOutcome};

/// Where the export goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetKind {
    /// A new `data-sources.json` file.
    File,
    /// A new DBeaver project folder.
    ProjectFolder,
}

/// Where the dialog stands.
#[derive(Debug)]
pub enum DbeaverExportStage {
    /// Choosing.
    Editing,
    /// The core is writing.
    Running,
    /// Written.
    Done(Box<DbeaverExportOutcome>),
    /// Refused or failed (the message); nothing was written.
    Failed(String),
}

/// The dialog's view.
pub struct DbeaverExportDialog {
    core: Option<CoreServices>,
    connections: Vec<(ConnectionId, String)>,
    selected: BTreeSet<ConnectionId>,
    kind: TargetKind,
    path: Entity<InputState>,
    stage: DbeaverExportStage,
    tasks: Vec<Task<()>>,
}

/// One line of the report.
#[must_use]
pub fn issue_line(issue: &ExportIssue) -> String {
    let kind = match issue.kind {
        ExportIssueKind::Dropped => "dropped",
        ExportIssueKind::Approximated => "approximated",
        ExportIssueKind::SecretOmitted => "secret not exported",
        ExportIssueKind::LooksLikeSecret => "looks like a secret, not written",
        ExportIssueKind::Skipped => "skipped",
        ExportIssueKind::NotVerified => "not verified",
        _ => "note",
    };
    format!(
        "{kind}: {}",
        crate::browse::display_message(&issue.to_string())
    )
}

impl DbeaverExportDialog {
    /// A dialog over the hub's saved connections (none selected).
    pub fn new(
        core: Option<CoreServices>,
        connections: Vec<(ConnectionId, String)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let path = cx.new(|cx| {
            InputState::new(window, cx).placeholder("/home/me/exports/data-sources-ninesql.json")
        });
        Self {
            core,
            connections,
            selected: BTreeSet::new(),
            kind: TargetKind::File,
            path,
            stage: DbeaverExportStage::Editing,
            tasks: Vec::new(),
        }
    }

    /// The stage.
    #[must_use]
    pub fn stage(&self) -> &DbeaverExportStage {
        &self.stage
    }

    /// The path input.
    #[must_use]
    pub fn path_input(&self) -> &Entity<InputState> {
        &self.path
    }

    /// Selects or unselects a connection.
    pub fn toggle(&mut self, id: ConnectionId, on: bool, cx: &mut Context<Self>) {
        if on {
            self.selected.insert(id);
        } else {
            self.selected.remove(&id);
        }
        cx.notify();
    }

    /// Selects every connection.
    pub fn select_all(&mut self, cx: &mut Context<Self>) {
        self.selected = self.connections.iter().map(|(id, _)| *id).collect();
        cx.notify();
    }

    /// Chooses the target kind.
    pub fn set_kind(&mut self, kind: TargetKind, cx: &mut Context<Self>) {
        self.kind = kind;
        cx.notify();
    }

    /// Why "Export" is disabled, if it is.
    #[must_use]
    pub fn start_problem(&self, cx: &App) -> Option<&'static str> {
        if self.core.is_none() {
            Some("The application core is not running.")
        } else if self.selected.is_empty() {
            Some("Choose at least one connection.")
        } else if self.path.read(cx).value().trim().is_empty() {
            Some(match self.kind {
                TargetKind::File => "Choose the new file to write.",
                TargetKind::ProjectFolder => "Choose the new project folder to create.",
            })
        } else {
            None
        }
    }

    /// "Choose…": the desktop's save dialog for a new file or a new folder name.
    pub fn choose(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dir = std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from);
        let name = match self.kind {
            TargetKind::File => "data-sources-ninesql.json",
            TargetKind::ProjectFolder => "NineSQL-export",
        };
        let answer = cx.prompt_for_new_path(&dir, Some(name));
        self.tasks.push(cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(path))) = answer.await {
                let _ = this.update_in(cx, |d, window, cx| {
                    let text = path.to_string_lossy().into_owned();
                    d.path.update(cx, |s, cx| s.set_value(text, window, cx));
                    cx.notify();
                });
            }
        }));
    }

    /// "Export".
    pub fn start(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.stage, DbeaverExportStage::Running) {
            return;
        }
        if let Some(p) = self.start_problem(cx) {
            self.stage = DbeaverExportStage::Failed(p.to_owned());
            cx.notify();
            return;
        }
        let Some(core) = self.core.clone() else {
            return;
        };
        let path = PathBuf::from(self.path.read(cx).value().trim());
        let target = match self.kind {
            TargetKind::File => ExportTarget::File(path),
            TargetKind::ProjectFolder => ExportTarget::ProjectFolder(path),
        };
        // In the list's order (the file keeps it).
        let ids: Vec<ConnectionId> = self
            .connections
            .iter()
            .map(|(id, _)| *id)
            .filter(|id| self.selected.contains(id))
            .collect();
        let reply = core.export_dbeaver(ids, target);
        self.stage = DbeaverExportStage::Running;
        cx.notify();
        self.tasks.push(cx.spawn_in(window, async move |this, cx| {
            let result = reply.await;
            let _ = this.update_in(cx, |d, window, cx| d.finish(result, window, cx));
        }));
    }

    fn finish(
        &mut self,
        result: Option<Result<DbeaverExportOutcome, DbeaverExportError>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.stage = match result {
            Some(Ok(outcome)) => DbeaverExportStage::Done(Box::new(outcome)),
            Some(Err(DbeaverExportError::Changed { .. })) => {
                // Nothing was written: the target must be chosen again.
                self.path.update(cx, |s, cx| s.set_value("", window, cx));
                DbeaverExportStage::Failed(
                    "The target changed while the export was written: nothing was exported. Choose the target again."
                        .to_owned(),
                )
            }
            Some(Err(e)) => {
                DbeaverExportStage::Failed(crate::browse::display_message(&e.to_string()))
            }
            None => DbeaverExportStage::Failed("The export task stopped.".to_owned()),
        };
        cx.notify();
    }

    /// Back to the choices after a refusal.
    pub fn back(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.stage, DbeaverExportStage::Running) {
            self.stage = DbeaverExportStage::Editing;
            cx.notify();
        }
    }

    fn render_report(&self, outcome: &DbeaverExportOutcome, cx: &App) -> Div {
        let muted = cx.theme().muted_foreground;
        let warn = crate::appearance::ensure_contrast(
            cx.theme().warning,
            &[cx.theme().background],
            crate::appearance::MIN_TEXT_CONTRAST,
        );
        let report = &outcome.report;
        let mut body = v_flex().gap_1().text_sm().child(format!(
            "Exported {} of {} connections to {}. DBeaver asks for the passwords: none was exported.",
            report.exported_count(),
            report.connections.len(),
            outcome.written.data_sources.display()
        ));
        for c in &report.connections {
            let head = if c.exported() {
                crate::browse::display_name(&c.name)
            } else {
                format!("{} (skipped)", crate::browse::display_name(&c.name))
            };
            body = body.child(div().child(head));
            for i in &c.issues {
                body = body.child(
                    div()
                        .pl_4()
                        .text_xs()
                        .text_color(if i.kind == ExportIssueKind::Skipped {
                            warn
                        } else {
                            muted
                        })
                        .child(issue_line(i)),
                );
            }
        }
        for i in &report.issues {
            body = body.child(div().text_xs().text_color(muted).child(issue_line(i)));
        }
        body
    }
}

impl Render for DbeaverExportDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let danger = crate::appearance::ensure_contrast(
            theme.danger,
            &[theme.background],
            crate::appearance::MIN_TEXT_CONTRAST,
        );
        if let DbeaverExportStage::Done(outcome) = &self.stage {
            return v_flex()
                .id("dbeaver-export-report")
                .max_h(px(480.))
                .overflow_y_scroll()
                .child(self.render_report(outcome, cx))
                .into_any_element();
        }
        let running = matches!(self.stage, DbeaverExportStage::Running);
        let rows = self.connections.iter().enumerate().map(|(i, (id, name))| {
            let id = *id;
            Checkbox::new(("dbeaver-export-conn", i))
                .label(crate::browse::display_name(name))
                .checked(self.selected.contains(&id))
                .disabled(running)
                .on_click(cx.listener(move |d, v: &bool, _, cx| d.toggle(id, *v, cx)))
        });
        let kind_button = |id: &'static str, label: &'static str, kind: TargetKind| {
            Button::new(id)
                .xsmall()
                .label(label)
                .when(self.kind == kind, |b| b.primary())
                .disabled(running)
                .on_click(cx.listener(move |d, _, _, cx| d.set_kind(kind, cx)))
        };
        let problem = self.start_problem(cx);
        v_flex()
            .gap_2()
            .text_sm()
            .child(div().text_color(theme.muted_foreground).child(
                "Writes a new DBeaver file without any password. Close DBeaver before copying it into a workspace.",
            ))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("dbeaver-export-all")
                            .xsmall()
                            .label("Select all")
                            .disabled(running)
                            .on_click(cx.listener(|d, _, _, cx| d.select_all(cx))),
                    )
                    .child(format!("{} selected", self.selected.len())),
            )
            .child(
                v_flex()
                    .id("dbeaver-export-list")
                    .max_h(px(220.))
                    .overflow_y_scroll()
                    .gap_1()
                    .when(self.connections.is_empty(), |d| {
                        d.child("No saved connection to export.")
                    })
                    .children(rows),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(kind_button("dbeaver-export-file", "New file", TargetKind::File))
                    .child(kind_button(
                        "dbeaver-export-folder",
                        "New project folder",
                        TargetKind::ProjectFolder,
                    )),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(div().flex_1().child(Input::new(&self.path)))
                    .child(
                        Button::new("dbeaver-export-choose")
                            .label("Choose…")
                            .disabled(running)
                            .on_click(cx.listener(|d, _, window, cx| d.choose(window, cx))),
                    ),
            )
            .when_some(
                match &self.stage {
                    DbeaverExportStage::Failed(m) => Some(m.clone()),
                    _ => None,
                },
                |d, m| d.child(div().id("dbeaver-export-error").text_color(danger).child(m)),
            )
            .child(
                h_flex().justify_end().child(
                    Button::new("dbeaver-export-start")
                        .primary()
                        .label(if running { "Exporting…" } else { "Export" })
                        .tooltip(problem.unwrap_or("Export"))
                        .disabled(running || problem.is_some())
                        .on_click(cx.listener(|d, _, window, cx| d.start(window, cx))),
                ),
            )
            .into_any_element()
    }
}

/// Opens the dialog over the hub's saved connections.
pub fn open_dbeaver_export(
    core: Option<CoreServices>,
    connections: Vec<(ConnectionId, String)>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<DbeaverExportDialog> {
    let dialog = cx.new(|cx| DbeaverExportDialog::new(core, connections, window, cx));
    let view = dialog.clone();
    window.open_dialog(cx, move |d, _, _| {
        d.title("Export connections to DBeaver")
            .w(px(620.))
            .child(view.clone())
    });
    dialog
}
