//! The DBeaver import flow of one window (P2-B, F-CON-14): preview, choices, apply, every
//! `ImportError`, and the outcome.
//!
//! - **Preview**: `CoreServices::preview_import` on the driver runtime's blocking pool, awaited
//!   on GPUI's executor (I6). The core reads the DBeaver files read-only; the UI never opens
//!   them (I4).
//! - **Choices**: per-connection skip, per-connection "accept the imported TLS settings" for an
//!   update that keeps a stronger saved setting (I5 warning), "Import saved passwords".
//! - **Test**: `ImportPreview::connection_for_test` (every credential on "ask", the init SQL
//!   marker kept) through the window's connect flow, under the types the import would save.
//! - **Apply**: through the hub (`ConnectionHub::apply_import`), which reloads its list from
//!   `ImportOutcome::connections`. `VaultLocked` opens the unlock prompt and `VaultNotCreated`
//!   the create prompt, then applies the same preview again; `SourceChanged` previews again
//!   with a notice.

use std::collections::BTreeMap;
use std::path::PathBuf;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use ninesql_core::{ImportCancel, ImportError, ImportPreview, ImportSource};

use super::model::{
    ConnectionRow, INIT_SQL_NOTE, OutcomeView, PreviewView, RowKind, Selection, TLS_ACCEPT_WARNING,
    TypeRowKind, store_label,
};
use crate::appearance::{MIN_TEXT_CONTRAST, ensure_contrast};
use crate::connections::connect_flow::{ConnectFlow, FlowFinished, FlowResult, Purpose};
use crate::connections::hub::ConnectionHub;
use crate::connections::vault::{VaultMode, VaultPrompt, VaultPromptEvent, open_vault_prompt};

/// Shown when the apply found the source changed and the preview was computed again.
pub const SOURCE_CHANGED_NOTICE: &str = "The DBeaver workspace or the saved connections changed since the preview: it was computed again. Review it, then import again.";

/// Shown when no workspace was found in the known locations.
pub const NOTHING_FOUND: &str = "No DBeaver workspace was found in the known locations (native, Flatpak, Snap). Choose its folder, or type its path.";

/// Where the flow is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stage {
    /// Nothing started.
    Idle,
    /// Waiting for a folder (nothing found, or "Choose folder").
    ChooseFolder,
    /// The core is reading the source.
    Previewing,
    /// The preview is shown.
    Preview,
    /// The core is writing.
    Applying,
    /// The vault prompt is open (unlock or create), then the import is applied again.
    WaitingForVault(VaultMode),
    /// Imported.
    Done(OutcomeView),
    /// The preview failed (the message never contains a secret).
    Failed(String),
}

/// State of the "Test" of one row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowTest {
    /// Running (or waiting for credentials).
    Running,
    /// Finished.
    Done(FlowResult),
}

/// The import flow and its screen.
pub struct ImportFlow {
    hub: Entity<ConnectionHub>,
    connect: Entity<ConnectFlow>,
    stage: Stage,
    source: Option<ImportSource>,
    preview: Option<ImportPreview>,
    view: Option<PreviewView>,
    selection: Selection,
    notice: Option<String>,
    tests: BTreeMap<String, RowTest>,
    testing: Option<String>,
    vault_prompt: Option<Entity<VaultPrompt>>,
    folder: Entity<InputState>,
    task: Option<Task<()>>,
    /// Stops the running preview in the core (P2-D M1).
    preview_cancel: Option<ImportCancel>,
    _subscriptions: Vec<Subscription>,
}

impl ImportFlow {
    /// A flow on `hub`, testing through the window's `connect` flow.
    pub fn new(
        hub: Entity<ConnectionHub>,
        connect: Entity<ConnectFlow>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let folder = cx.new(|cx| {
            InputState::new(window, cx).placeholder("/home/you/.local/share/DBeaverData/workspace6")
        });
        let sub = cx.subscribe(&connect, |this: &mut Self, _, event: &FlowFinished, cx| {
            if event.purpose == Purpose::Test
                && let Some(id) = this.testing.take()
            {
                this.tests.insert(id, RowTest::Done(event.result.clone()));
                cx.notify();
            }
        });
        Self {
            hub,
            connect,
            stage: Stage::Idle,
            source: None,
            preview: None,
            view: None,
            selection: Selection::default(),
            notice: None,
            tests: BTreeMap::new(),
            testing: None,
            vault_prompt: None,
            folder,
            task: None,
            preview_cancel: None,
            _subscriptions: vec![sub],
        }
    }

    /// Where the flow is.
    #[must_use]
    pub fn stage(&self) -> &Stage {
        &self.stage
    }

    /// The preview view model, once previewed.
    #[must_use]
    pub fn view(&self) -> Option<&PreviewView> {
        self.view.as_ref()
    }

    /// The preview from the core.
    #[must_use]
    pub fn preview(&self) -> Option<&ImportPreview> {
        self.preview.as_ref()
    }

    /// The user's choices.
    #[must_use]
    pub fn selection(&self) -> &Selection {
        &self.selection
    }

    /// The notice shown above the preview (source changed, vault not unlocked, apply error).
    #[must_use]
    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    /// The "Test" state of a row.
    #[must_use]
    pub fn test_state(&self, dbeaver_id: &str) -> Option<&RowTest> {
        self.tests.get(dbeaver_id)
    }

    /// The vault prompt opened by the apply, while open (tests type into it).
    #[must_use]
    pub fn vault_prompt(&self) -> Option<Entity<VaultPrompt>> {
        self.vault_prompt.clone()
    }

    /// The folder path input.
    #[must_use]
    pub fn folder_input(&self) -> &Entity<InputState> {
        &self.folder
    }

    /// The cancel handle of the running preview (P2-D M1), while it runs.
    #[must_use]
    pub fn preview_cancel(&self) -> Option<&ImportCancel> {
        self.preview_cancel.as_ref()
    }

    /// Whether a "Test" of this flow is running in the connect flow.
    #[must_use]
    pub fn is_testing(&self) -> bool {
        self.testing.is_some()
    }

    /// Whether the core is working (preview or apply) or a prompt is open.
    #[must_use]
    pub fn is_busy(&self) -> bool {
        matches!(
            self.stage,
            Stage::Previewing | Stage::Applying | Stage::WaitingForVault(_)
        )
    }

    /// Waits for a folder ("Choose folder…", or nothing in the known locations).
    pub fn choose_folder_stage(&mut self, notice: Option<String>, cx: &mut Context<Self>) {
        self.stage = Stage::ChooseFolder;
        self.notice = notice;
        cx.notify();
    }

    /// Previews `source` (known locations or a folder). Refused while the flow is busy: a
    /// running apply must keep its task (P2-D review, L2b).
    pub fn start(&mut self, source: ImportSource, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_busy() {
            return;
        }
        let Some(core) = self.hub.read(cx).core().cloned() else {
            self.stage = Stage::Failed(
                "NineSQL is still starting, or its core did not start: try again in a moment."
                    .to_owned(),
            );
            cx.notify();
            return;
        };
        let (reply, cancel) = core.preview_import_cancellable(source.clone());
        self.preview_cancel = Some(cancel);
        self.source = Some(source);
        self.stage = Stage::Previewing;
        cx.notify();
        self.task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = reply.await;
            let _ = this.update_in(cx, |f, window, cx| f.on_preview(result, window, cx));
        }));
    }

    /// Previews the known locations under `$HOME`; without `HOME`, asks for a folder.
    pub fn start_detect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match ImportSource::detect_from_env() {
            Some(source) => self.start(source, window, cx),
            None => self.choose_folder_stage(Some(NOTHING_FOUND.to_owned()), cx),
        }
    }

    /// Previews the folder typed in the path input.
    pub fn start_typed_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.folder.read(cx).value().trim().to_owned();
        let path = PathBuf::from(&text);
        if text.is_empty() || !path.is_absolute() {
            self.notice =
                Some("Type the absolute path of a DBeaver workspace or project folder.".to_owned());
            cx.notify();
            return;
        }
        self.start(ImportSource::Folder(path), window, cx);
    }

    /// "Choose folder…": the desktop's folder picker (XDG portal on Linux), then the preview.
    pub fn choose_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_busy() {
            return;
        }
        if !matches!(self.stage, Stage::Preview) {
            self.stage = Stage::ChooseFolder;
        }
        let answer = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Import from this folder".into()),
        });
        cx.notify();
        self.task = Some(cx.spawn_in(window, async move |this, cx| {
            let answer = answer.await;
            let _ = this.update_in(cx, |f, window, cx| match answer {
                Ok(Ok(Some(paths))) => {
                    if let Some(path) = paths.into_iter().next() {
                        f.start(ImportSource::Folder(path), window, cx);
                    }
                }
                Ok(Err(e)) => {
                    f.notice = Some(format!(
                        "The folder picker could not be opened ({e}): type the folder path instead."
                    ));
                    cx.notify();
                }
                _ => {}
            });
        }));
    }

    /// "Cancel" while the core reads the source: stops the core's read
    /// (`ImportCancel::cancel`, it stops between DBeaver projects), drops the pending preview
    /// and returns to idle. Nothing is written either way (I4).
    pub fn cancel_preview(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.stage, Stage::Previewing) {
            return;
        }
        if let Some(cancel) = self.preview_cancel.take() {
            cancel.cancel();
        }
        self.task = None;
        self.source = None;
        self.preview = None;
        self.view = None;
        self.tests.clear();
        self.selection.accept_tls.clear();
        self.notice = None;
        self.stage = Stage::Idle;
        cx.notify();
    }

    /// Applies a preview answer (public for tests that drive the flow step by step).
    pub fn on_preview(
        &mut self,
        result: Option<Result<ImportPreview, ImportError>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.preview_cancel = None;
        match result {
            Some(Ok(preview)) => {
                let view = {
                    let hub = self.hub.read(cx);
                    PreviewView::build(&preview, hub.types(), hub.connections())
                };
                // Skips survive a new preview; an accepted TLS change never does (I5: the
                // user accepts what is shown, not what was shown before the source changed).
                self.selection
                    .skip
                    .retain(|id| view.row(id).is_some_and(|r| r.skippable));
                self.selection.accept_tls.clear();
                if view.default_store.is_none() {
                    self.selection.import_secrets = false;
                }
                self.tests.clear();
                self.view = Some(view);
                self.preview = Some(preview);
                self.stage = Stage::Preview;
            }
            Some(Err(ImportError::NothingFound)) => {
                self.stage = Stage::ChooseFolder;
                self.notice = Some(NOTHING_FOUND.to_owned());
            }
            // A cancel is not a failure: back to idle, nothing to report.
            Some(Err(ImportError::Cancelled)) => {
                self.source = None;
                self.stage = Stage::Idle;
                self.notice = Some("Import preview cancelled: nothing was imported.".to_owned());
            }
            Some(Err(e @ ImportError::ListingTruncated)) => {
                self.stage = Stage::ChooseFolder;
                self.notice = Some(format!("Cannot preview: {e}."));
            }
            Some(Err(e)) => {
                self.stage = Stage::Failed(format!("Cannot read the DBeaver workspace: {e}"))
            }
            None => self.stage = Stage::Failed("The preview stopped unexpectedly.".to_owned()),
        }
        cx.notify();
    }

    /// Leaves a connection out, or takes it back.
    pub fn toggle_skip(&mut self, dbeaver_id: &str, cx: &mut Context<Self>) {
        if !self
            .view
            .as_ref()
            .and_then(|v| v.row(dbeaver_id))
            .is_some_and(|r| r.skippable)
        {
            return;
        }
        if !self.selection.skip.remove(dbeaver_id) {
            self.selection.skip.insert(dbeaver_id.to_owned());
        }
        cx.notify();
    }

    /// Accepts the imported TLS settings of an update that keeps the saved ones, or not.
    pub fn toggle_accept_tls(&mut self, dbeaver_id: &str, cx: &mut Context<Self>) {
        if !self
            .view
            .as_ref()
            .and_then(|v| v.row(dbeaver_id))
            .is_some_and(ConnectionRow::tls_kept)
        {
            return;
        }
        if !self.selection.accept_tls.remove(dbeaver_id) {
            self.selection.accept_tls.insert(dbeaver_id.to_owned());
        }
        cx.notify();
    }

    /// "Import saved passwords" (only with a store).
    pub fn set_import_secrets(&mut self, on: bool, cx: &mut Context<Self>) {
        let has_store = self
            .view
            .as_ref()
            .is_some_and(|v| v.default_store.is_some());
        self.selection.import_secrets = on && has_store;
        cx.notify();
    }

    /// "Update connections imported before".
    pub fn set_update_existing(&mut self, on: bool, cx: &mut Context<Self>) {
        self.selection.update_existing = on;
        cx.notify();
    }

    /// "Test": opens the planned connection through the connect flow (credentials always
    /// asked), under the types the import would save.
    pub fn test(&mut self, dbeaver_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(preview) = &self.preview else {
            return;
        };
        let Some(connection) = preview.connection_for_test(dbeaver_id) else {
            self.notice = Some(
                "This connection cannot be tested: it is not imported, or imported disabled."
                    .to_owned(),
            );
            cx.notify();
            return;
        };
        if self.connect.read(cx).is_busy() {
            self.notice = Some("A connection is already being opened.".to_owned());
            cx.notify();
            return;
        }
        let types = preview.types_after(self.hub.read(cx).types());
        self.testing = Some(dbeaver_id.to_owned());
        self.tests.insert(dbeaver_id.to_owned(), RowTest::Running);
        self.connect.update(cx, |flow, cx| {
            flow.start_with_types(
                connection,
                Purpose::Test,
                Vec::new(),
                Some(types),
                window,
                cx,
            );
        });
        cx.notify();
    }

    /// Applies the preview with the choices.
    pub fn apply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_busy() {
            return;
        }
        let (Some(preview), Some(view)) = (self.preview.clone(), self.view.as_ref()) else {
            return;
        };
        let choices = self.selection.choices(view);
        self.stage = Stage::Applying;
        self.notice = None;
        let task = self
            .hub
            .update(cx, |hub, cx| hub.apply_import(preview, choices, cx));
        cx.notify();
        self.task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |f, window, cx| f.on_apply(result, window, cx));
        }));
    }

    /// Applies an apply answer (public for tests).
    pub fn on_apply(
        &mut self,
        result: Result<ninesql_core::ImportOutcome, ImportError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(outcome) => {
                let had = self.preview.as_ref().is_some_and(|p| p.secret_count() > 0);
                self.stage = Stage::Done(OutcomeView::build(&outcome, had));
                self.notice = None;
            }
            Err(ImportError::VaultLocked) => self.ask_vault(VaultMode::Unlock, window, cx),
            Err(ImportError::VaultNotCreated) => self.ask_vault(VaultMode::Create, window, cx),
            Err(ImportError::SourceChanged) => {
                self.notice = Some(SOURCE_CHANGED_NOTICE.to_owned());
                // Out of `Applying` first: `start` is refused while busy.
                self.stage = Stage::Preview;
                // The notice stays over the new preview (`start` keeps it).
                if let Some(source) = self.source.clone() {
                    self.start(source, window, cx);
                }
            }
            Err(ImportError::NoStore(kind)) => {
                self.stage = Stage::Preview;
                self.notice = Some(format!(
                    "Not imported: the {kind} is not available in this session. Turn off \"Import saved passwords\" to import without them (NineSQL then asks for them at connect)."
                ));
            }
            Err(e) => {
                self.stage = Stage::Preview;
                self.notice = Some(format!("Not imported: {e}"));
            }
        }
        cx.notify();
    }

    fn ask_vault(&mut self, mode: VaultMode, window: &mut Window, cx: &mut Context<Self>) {
        if self.hub.read(cx).core().is_none() {
            self.stage = Stage::Preview;
            self.notice = Some("The vault cannot be used in this session.".to_owned());
            return;
        }
        let prompt = open_vault_prompt(self.hub.clone(), mode, window, cx);
        let sub = cx.subscribe_in(&prompt, window, |this, _, event, window, cx| {
            if this.vault_prompt.take().is_none() {
                return;
            }
            match event {
                VaultPromptEvent::Done => {
                    this.stage = Stage::Preview;
                    // The same preview, again (nothing was written).
                    this.apply(window, cx);
                }
                VaultPromptEvent::Cancelled => {
                    this.stage = Stage::Preview;
                    this.notice = Some(
                        "Not imported: the vault is needed for the saved passwords. Unlock it, or turn off \"Import saved passwords\"."
                            .to_owned(),
                    );
                    cx.notify();
                }
            }
        });
        self._subscriptions.push(sub);
        self.vault_prompt = Some(prompt);
        self.stage = Stage::WaitingForVault(mode);
    }

    /// The dialog was closed: a running test or prompt is left to finish on its own; nothing
    /// was written unless the import was applied.
    pub fn dismissed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(prompt) = self.vault_prompt.take() {
            prompt.update(cx, |p, cx| p.dismissed(window, cx));
            // The prompt's `Cancelled` no longer reaches us (the subscription sees no prompt):
            // leave the waiting stage here, or the flow stays busy (P2-D review, L2a).
            if matches!(self.stage, Stage::WaitingForVault(_)) {
                self.stage = Stage::Preview;
                self.notice = Some(
                    "Not imported: the vault prompt was closed. Nothing was written.".to_owned(),
                );
            }
        }
        self.testing = None;
        cx.notify();
    }
}

fn section(title: &'static str) -> Div {
    div()
        .pt_2()
        .text_sm()
        .font_weight(FontWeight::SEMIBOLD)
        .child(title)
}

impl ImportFlow {
    fn render_row(&self, row: &ConnectionRow, i: usize, cx: &mut Context<Self>) -> Stateful<Div> {
        let theme = cx.theme();
        let bg = theme.background;
        let muted = theme.muted_foreground;
        let warning = ensure_contrast(theme.warning, &[bg], MIN_TEXT_CONTRAST);
        let danger = ensure_contrast(theme.danger, &[bg], MIN_TEXT_CONTRAST);
        let success = ensure_contrast(theme.success, &[bg], MIN_TEXT_CONTRAST);
        let id = row.dbeaver_id.clone();
        let skipped = self.selection.skip.contains(&row.dbeaver_id);
        let accepted = self.selection.accept_tls.contains(&row.dbeaver_id);
        let mut details: Vec<String> = Vec::new();
        if let Some(e) = &row.engine {
            details.push(e.clone());
        }
        if let Some(t) = &row.type_name {
            details.push(format!("type {t}"));
        }
        if let Some(f) = &row.folder {
            details.push(format!("folder {f}"));
        }
        let create_tls = row.create_tls.clone();
        let header = h_flex()
            .gap_2()
            .when(row.skippable, |el| {
                let id = id.clone();
                el.child(
                    Checkbox::new(("import-row", i))
                        .checked(!skipped)
                        .accessibility_label(format!("Import {}", row.name))
                        .on_click(cx.listener(move |this, _, _, cx| this.toggle_skip(&id, cx))),
                )
            })
            .child(div().font_weight(FontWeight::SEMIBOLD).child(row.name.clone()))
            .child(
                div()
                    .id(("row-kind", i))
                    .text_xs()
                    .text_color(match row.kind {
                        RowKind::Create | RowKind::Update => success,
                        RowKind::Disabled => warning,
                        RowKind::NotImported => danger,
                    })
                    .child(if row.updates && row.kind == RowKind::Disabled {
                        "update, imported disabled".to_owned()
                    } else {
                        row.kind.label().to_owned()
                    }),
            )
            .child(div().text_xs().text_color(muted).child(details.join(" · ")))
            .when_some(create_tls, |el, tls| {
                el.child(
                    div()
                        .id(("row-create-tls", i))
                        .text_xs()
                        .text_color(if tls.weak { warning } else { muted })
                        .child(format!("TLS {}", tls.text)),
                )
            })
            .when(row.testable, |el| {
                let id = id.clone();
                el.child(
                    Button::new(("row-test", i))
                        .xsmall()
                        .outline()
                        .label("Test")
                        .tooltip("Opens the connection as it would be imported; credentials are asked, never taken from DBeaver.")
                        .on_click(cx.listener(move |this, _, window, cx| this.test(&id, window, cx))),
                )
            });
        let test_line = self.tests.get(&row.dbeaver_id).map(|t| match t {
            RowTest::Running => ("Testing…".to_owned(), muted),
            RowTest::Done(FlowResult::TestOk { server_version, .. }) => (
                format!(
                    "Test succeeded{}.",
                    server_version
                        .as_ref()
                        .map(|v| format!(": {v}"))
                        .unwrap_or_default()
                ),
                success,
            ),
            RowTest::Done(FlowResult::Failed(e)) => (format!("Test failed: {e}"), danger),
            RowTest::Done(FlowResult::Cancelled) => ("Test cancelled.".to_owned(), muted),
            RowTest::Done(FlowResult::Opened { .. }) => (String::new(), muted),
        });
        let diff = row.diff.clone();
        v_flex()
            .id(("import-connection", i))
            .gap_1()
            .py_1()
            .border_b_1()
            .border_color(theme.border)
            .child(header)
            .when_some(row.reason.clone(), |el, reason| {
                let label = if row.kind == RowKind::NotImported {
                    "Not imported"
                } else {
                    "Disabled"
                };
                el.child(
                    div()
                        .id(("row-reason", i))
                        .text_xs()
                        .text_color(if row.kind == RowKind::NotImported { danger } else { warning })
                        .child(format!("{label}: {reason}")),
                )
            })
            .when(!row.secrets.is_empty(), |el| {
                el.child(
                    div()
                        .id(("row-secrets", i))
                        .text_xs()
                        .child(format!("Saved secrets to move: {}", row.secrets.join(", "))),
                )
            })
            .when(row.init_sql > 0, |el| {
                el.child(
                    div()
                        .id(("row-init-sql", i))
                        .text_xs()
                        .text_color(warning)
                        .child(format!("{} init SQL statement(s): {INIT_SQL_NOTE}.", row.init_sql)),
                )
            })
            .when_some(diff, |el, diff| {
                el.children(diff.changes.iter().enumerate().map(|(j, c)| {
                    div()
                        .id(SharedString::from(format!("row-diff-{i}-{j}")))
                        .text_xs()
                        .child(c.clone())
                }))
                .when_some(diff.tls.clone(), |el, tls| {
                    let id = id.clone();
                    el.child(
                        div()
                            .id(("row-tls", i))
                            .text_xs()
                            .text_color(if tls.kept_saved { warning } else { muted })
                            .child(if tls.kept_saved {
                                format!(
                                    "TLS: the saved setting ({}) is kept; DBeaver has {}.",
                                    tls.saved, tls.imported
                                )
                            } else {
                                format!("TLS: {} → {}", tls.saved, tls.imported)
                            }),
                    )
                    .when(tls.kept_saved, |el| {
                        el.child(
                            Checkbox::new(("row-accept-tls", i))
                                .checked(accepted)
                                .label("Accept the imported TLS settings")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.toggle_accept_tls(&id, cx);
                                })),
                        )
                        // Shown before the box is ticked too (P2-D review, L3).
                        .child(
                            div()
                                .id(("row-tls-warning", i))
                                .text_xs()
                                .text_color(if accepted { danger } else { warning })
                                .child(TLS_ACCEPT_WARNING),
                        )
                    })
                })
                .when(!diff.secrets_dropped.is_empty(), |el| {
                    el.child(
                        div()
                            .id(("row-secrets-dropped", i))
                            .text_xs()
                            .text_color(warning)
                            .child(format!(
                                "Saved secrets dropped (asked at connect): {}. What they authenticate against changed.",
                                diff.secrets_dropped.join(", ")
                            )),
                    )
                })
                .when(!diff.secrets_replaced.is_empty(), |el| {
                    el.child(div().text_xs().child(format!(
                        "Saved secrets replaced by DBeaver's: {}",
                        diff.secrets_replaced.join(", ")
                    )))
                })
            })
            .children(row.issues.iter().enumerate().map(|(j, issue)| {
                div()
                    .id(SharedString::from(format!("row-issue-{i}-{j}")))
                    .text_xs()
                    .text_color(muted)
                    .child(issue.text())
            }))
            .when_some(test_line, |el, (text, color)| {
                el.child(div().id(("row-test-result", i)).text_xs().text_color(color).child(text))
            })
    }

    fn render_folder_chooser(&self, cx: &mut Context<Self>) -> Div {
        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .child(div().flex_1().child(Input::new(&self.folder)))
                    .child(
                        Button::new("import-folder-preview")
                            .small()
                            .label("Preview")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.start_typed_folder(window, cx);
                            })),
                    )
                    .child(
                        Button::new("import-choose-folder")
                            .small()
                            .label("Choose folder…")
                            .on_click(cx.listener(|this, _, window, cx| this.choose_folder(window, cx))),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child("A DBeaver workspace (for example workspace6) or one project folder. NineSQL only reads it."),
            )
    }
}

impl Render for ImportFlow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let bg = theme.background;
        let muted = theme.muted_foreground;
        let warning = ensure_contrast(theme.warning, &[bg], MIN_TEXT_CONTRAST);
        let danger = ensure_contrast(theme.danger, &[bg], MIN_TEXT_CONTRAST);
        let notice = self.notice.clone();
        let body = v_flex()
            .id("import-body")
            .key_context("ImportFromDbeaver")
            .gap_2()
            .text_sm()
            .max_h(px(600.))
            .overflow_y_scroll()
            .pr_2()
            .when_some(notice, |el, n| {
                el.child(div().id("import-notice").text_color(warning).child(n))
            });
        let stage = self.stage.clone();
        let (body, footer): (Stateful<Div>, Div) = match stage {
            Stage::Idle | Stage::ChooseFolder => (
                body.child(self.render_folder_chooser(cx)),
                h_flex().gap_2().justify_end().child(close_button("Cancel")),
            ),
            Stage::Previewing => (
                body.child(
                    div()
                        .text_color(muted)
                        .child("Reading the DBeaver workspace…"),
                ),
                h_flex().gap_2().justify_end().child(
                    Button::new("import-cancel-preview")
                        .label("Cancel")
                        .on_click(cx.listener(|this, _, _, cx| this.cancel_preview(cx))),
                ),
            ),
            Stage::Failed(e) => (
                body.child(div().id("import-error").text_color(danger).child(e))
                    .child(self.render_folder_chooser(cx)),
                h_flex().gap_2().justify_end().child(close_button("Close")),
            ),
            Stage::Applying | Stage::WaitingForVault(_) => (
                body.child(div().text_color(muted).child("Importing…")),
                h_flex(),
            ),
            Stage::Done(outcome) => (
                body.child(section("Imported"))
                    .children(
                        outcome
                            .lines
                            .iter()
                            .enumerate()
                            .map(|(i, l)| div().id(("import-outcome", i)).child(l.clone())),
                    )
                    .child(match (&outcome.report_file, &outcome.report_error) {
                        (Some(file), _) => div()
                            .id("import-report-file")
                            .text_xs()
                            .child(format!("Report saved to {file}")),
                        (None, error) => div()
                            .id("import-report-error")
                            .text_xs()
                            .text_color(warning)
                            .child(format!(
                                "Import {}. The connections were imported.",
                                error.as_deref().unwrap_or("report not saved")
                            )),
                    })
                    .when_some(outcome.advice, |el, advice| {
                        el.child(div().id("import-advice").text_color(warning).child(advice))
                    }),
                h_flex().gap_2().justify_end().child(close_button("Close")),
            ),
            Stage::Preview => {
                let Some(view) = self.view.clone() else {
                    return v_flex().into_any_element();
                };
                let selected = self.selection.selected_count(&view);
                let store = if self.selection.import_secrets {
                    view.default_store
                } else {
                    None
                };
                let has_updates = view.rows().any(|r| r.updates);
                let source_issues: Vec<String> = view.source_issues().map(|i| i.text()).collect();
                let mut body = body
                    .when(view.truncated, |el| {
                        el.child(
                            div()
                                .id("import-truncated")
                                .p_2()
                                .border_1()
                                .border_color(warning)
                                .text_color(warning)
                                .font_weight(FontWeight::SEMIBOLD)
                                .child("Incomplete preview: a limit was reached while reading the DBeaver workspace, so some projects or connections are not listed and will not be imported. Pick a smaller folder (one project) to import the rest."),
                        )
                    })
                    .when(!source_issues.is_empty(), |el| {
                        el.child(section("Source issues")).children(
                            source_issues.iter().enumerate().map(|(i, t)| {
                                div()
                                    .id(("import-source-issue", i))
                                    .text_xs()
                                    .text_color(warning)
                                    .child(t.clone())
                            }),
                        )
                    })
                    .child(section("Source"))
                    .children(view.sources.iter().enumerate().map(|(i, s)| {
                        div().id(("import-source", i)).text_xs().child(s.clone())
                    }))
                    .child(section("Saved passwords"))
                    .child(div().id("import-secrets").child(if view.secret_count == 0 {
                        "No saved password to move.".to_owned()
                    } else if store.is_some() {
                        format!(
                            "{} saved secret(s) move to {}. Values are never shown.",
                            view.secret_count,
                            store_label(store)
                        )
                    } else if view.default_store.is_none() {
                        format!(
                            "{} saved secret(s) are not imported: {}. NineSQL asks for them at connect.",
                            view.secret_count,
                            store_label(None)
                        )
                    } else {
                        format!(
                            "{} saved secret(s) are not imported: NineSQL asks for them at connect.",
                            view.secret_count
                        )
                    }))
                    .when(view.secret_count > 0 && view.default_store.is_some(), |el| {
                        let on = self.selection.import_secrets;
                        el.child(
                            Checkbox::new("import-secrets-toggle")
                                .checked(on)
                                .label("Import saved passwords")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.set_import_secrets(!on, cx);
                                })),
                        )
                    })
                    .when(has_updates, |el| {
                        let on = self.selection.update_existing;
                        el.child(
                            Checkbox::new("import-update-existing")
                                .checked(on)
                                .label("Update connections imported before")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.set_update_existing(!on, cx);
                                })),
                        )
                    });
                if !view.types.is_empty() {
                    body = body.child(section("Connection types")).children(
                        view.types.iter().enumerate().map(|(i, t)| {
                            let color = if t.changes_existing()
                                || (t.kind == TypeRowKind::Tighten && t.builtin)
                            {
                                warning
                            } else {
                                cx.theme().foreground
                            };
                            div()
                                .id(("import-type", i))
                                .text_color(color)
                                .when(t.kind == TypeRowKind::Tighten, |el| {
                                    el.font_weight(FontWeight::SEMIBOLD)
                                })
                                .child(t.text())
                        }),
                    );
                }
                if !view.issues.is_empty() {
                    body = body.children(view.issues.iter().enumerate().map(|(i, issue)| {
                        div()
                            .id(("import-issue", i))
                            .text_xs()
                            .text_color(muted)
                            .child(issue.text())
                    }));
                }
                let mut row_ix = 0usize;
                for (p_ix, p) in view.projects.iter().enumerate() {
                    body = body
                        .child(
                            div()
                                .id(("import-project", p_ix))
                                .pt_2()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(format!(
                                    "Project {} ({} connection(s))",
                                    p.name,
                                    p.connections.len()
                                )),
                        )
                        .child(div().text_xs().text_color(muted).child(p.path.clone()))
                        .child(div().text_xs().child(p.credentials.clone()))
                        .when(!p.unused_folders.is_empty(), |el| {
                            el.child(div().text_xs().text_color(muted).child(format!(
                                "Empty folders, not created: {}",
                                p.unused_folders.join(", ")
                            )))
                        })
                        .children(
                            p.issues
                                .iter()
                                .map(|i| div().text_xs().text_color(muted).child(i.text())),
                        );
                    for row in &p.connections {
                        body = body.child(self.render_row(row, row_ix, cx));
                        row_ix += 1;
                    }
                }
                let footer = h_flex()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("import-choose-other")
                            .label("Choose folder…")
                            .on_click(
                                cx.listener(|this, _, window, cx| this.choose_folder(window, cx)),
                            ),
                    )
                    .child(close_button("Cancel"))
                    .child(
                        Button::new("import-apply")
                            .primary()
                            .label(format!("Import {selected} connection(s)"))
                            .disabled(selected == 0)
                            .on_click(cx.listener(|this, _, window, cx| this.apply(window, cx))),
                    );
                (body, footer)
            }
        };
        v_flex()
            .gap_2()
            .child(body)
            .child(footer)
            .into_any_element()
    }
}

fn close_button(label: &'static str) -> Button {
    Button::new("import-close")
        .label(label)
        .on_click(|_, window, cx| window.close_dialog(cx))
}

/// Opens the import screen in a dialog.
pub fn open_import_dialog(flow: Entity<ImportFlow>, window: &mut Window, cx: &mut App) {
    window.open_dialog(cx, move |dialog, _window, _cx| {
        let on_close = flow.clone();
        dialog
            .title("Import from DBeaver")
            .w(px(860.))
            .on_close(move |_, window, cx| {
                on_close.update(cx, |f, cx| f.dismissed(window, cx));
            })
            .child(flow.clone())
    });
}
