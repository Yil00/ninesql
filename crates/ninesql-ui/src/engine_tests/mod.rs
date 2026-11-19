//! `TestAppContext` tests of the UI against the real drivers and the dev engines (ADR-0029
//! step 1, ROADMAP P6-G): the shipped `pgwire`, `mysqlwire` and `sqlite` drivers against the dev
//! PostgreSQL, MySQL and MariaDB of `dev/compose.yaml` and a SQLite file created per test.
//!
//! - The tests drive the application through its registered actions (the palette path,
//!   `Workspace::perform`) and the view models' public state: the connection dialog's inputs
//!   and "Test connection", the navigator's rows and context menu, the table tab's grid and
//!   toolbar, the export and import dialogs, the statement confirmation. They never call a core
//!   service to make a step pass; a separate driver session ([`harness::Admin`]) creates the
//!   scratch tables and reads values back.
//! - Skip rule (AGENTS.md): a test is skipped (not failed) when its engine is unreachable,
//!   unless `NINESQL_REQUIRE_ENGINES` lists it. SQLite needs no container and always runs.
//! - Secrets: an in-memory store and the throwaway passwords of `dev/engines.json`; every test
//!   ends with the I1 scan of its temporary XDG directories.
//! - Scratch tables are named per test and per run (`ui_e2e_<what>_<uuid>`) and dropped at the
//!   end, so the tests can run in parallel with each other and with the driver tests.
//!
//! Start the engines with `python3 dev/ready.py up core` (or `make dev-up`), then
//! `NINESQL_REQUIRE_ENGINES=postgresql,mysql,mariadb cargo test -p ninesql-ui engine_tests`.

mod harness;

use gpui::TestAppContext;
use gpui_kit::component::WindowExt as _;
use ninesql_core::ConnectionTypeId;
use ninesql_core::driver_api::ObjectKind;
use ninesql_grid::{FetchState, ResultTab};

use harness::{Admin, App, Engine, target, wait};

/// Rows of the paged scratch table: more than one page of the grid.
const PAGED_ROWS: u32 = 10_000;

fn start(cx: &mut TestAppContext, engine: Engine) -> Option<(App, tempfile::TempDir)> {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let target = target(engine, tmp.path())?;
    let app = App::start(cx, target, tmp.path());
    Some((app, tmp))
}

/// The rows loaded in the first result tab of `tab`, and its fetch state.
fn first_result(
    app: &App,
    tab: &gpui::Entity<crate::editor_tab::SqlEditorTab>,
) -> Option<(usize, FetchState)> {
    tab.read_with(&app.vcx, |t, cx| {
        match t.results().read(cx).results().tabs().first() {
            Some(ResultTab::Table(m)) => {
                let m = m.borrow();
                Some((m.row_count(), m.state()))
            }
            _ => None,
        }
    })
}

fn message(app: &App, tab: &gpui::Entity<crate::editor_tab::SqlEditorTab>) -> String {
    tab.read_with(&app.vcx, |t, _| t.message().map(|m| m.text.to_string()))
        .unwrap_or_default()
}

/// The statements of `tab`'s last run that reported an update count.
fn update_counts(app: &App, tab: &gpui::Entity<crate::editor_tab::SqlEditorTab>) -> Vec<u32> {
    tab.read_with(&app.vcx, |t, cx| {
        t.results()
            .read(cx)
            .results()
            .update_counts()
            .iter()
            .map(|u| u.statement)
            .collect()
    })
}

/// Runs `text` in `tab`'s editor with "Execute statement" (`ctrl-enter`).
fn execute(app: &mut App, tab: &gpui::Entity<crate::editor_tab::SqlEditorTab>, text: &str) {
    let editor = tab.read_with(&app.vcx, |t, _| t.editor().clone());
    let text = text.to_owned();
    app.vcx
        .update(|window, cx| editor.update(cx, |e, cx| e.set_text(&text, window, cx)));
    app.perform(&ninesql_editor::actions::ExecuteStatement);
}

/// The navigator path to a table.
fn table_path<'a>(engine: Engine, connection: &'a str, table: &'a str) -> Vec<&'a str> {
    vec![connection, engine.schema(), "Tables", table]
}

/// Opens the navigator's selected table with the first entry of its context menu, which must
/// be "Open table data"; returns the new tab.
fn open_table_from_menu(app: &mut App) -> gpui::Entity<crate::editor_tab::SqlEditorTab> {
    let entries = app
        .nav
        .read_with(&app.vcx, |n, cx| n.selected_object_menu(cx));
    let Some(Some((label, action))) = entries.into_iter().next() else {
        panic!("a first context-menu entry");
    };
    assert_eq!(label, "Open table data");
    let before = app.ws.read_with(&app.vcx, |ws, _| {
        ws.active_editor().map(gpui::Entity::entity_id)
    });
    app.perform(action.as_ref());
    let tab = app
        .ws
        .read_with(&app.vcx, |ws, _| ws.active_editor().cloned());
    let Some(tab) = tab else {
        panic!("a table tab");
    };
    assert_ne!(Some(tab.entity_id()), before, "a new tab");
    assert!(tab.read_with(&app.vcx, |t, _| t.table_state().source.is_some()));
    tab
}

// ---------------------------------------------------------------------------------------------
// Scenarios 1 to 3: connect, the navigator, "Open table data" on a paged table, edit, "Cancel",
// save, read back.

fn connect_browse_edit_paged(cx: &mut TestAppContext, engine: Engine) {
    let Some((mut app, _tmp)) = start(cx, engine) else {
        return;
    };
    let mut admin = Admin::connect(&app.target);
    let table = admin.scratch_table("paged", PAGED_ROWS);

    // 1. Connect: the dialog, "Test connection", "Save", then "Connect" from the navigator.
    let name = format!("Dev {}", engine.id());
    let id = app.create_connection(&name, ConnectionTypeId::dev(), false);
    app.connect(id);
    let session = app
        .hub
        .read_with(&app.vcx, |h, _| h.sessions(id).first().cloned())
        .unwrap_or_else(|| panic!("a session"));
    assert!(!session.info().read_only);
    let caps = session.info().capabilities.clone();

    // 2. The navigator down to a table of the "nine" dataset; its context menu starts with
    // "Open table data", and "Show DDL" follows the declared DDL kinds (I8).
    app.reveal(&table_path(engine, &name, "customers"));
    let entries = app
        .nav
        .read_with(&app.vcx, |n, cx| n.selected_object_menu(cx));
    let labels: Vec<String> = entries
        .iter()
        .map(|e| {
            e.as_ref()
                .map_or_else(|| "-".to_owned(), |(l, _)| l.clone())
        })
        .collect();
    assert_eq!(labels.first().map(String::as_str), Some("Open table data"));
    let ddl = app.hub.read_with(&app.vcx, |h, _| {
        h.metadata()
            .is_some_and(|m| m.supports_ddl(id, ObjectKind::Table))
    });
    assert_eq!(labels.iter().any(|l| l == "Show DDL"), ddl, "{labels:?}");
    assert!(
        labels.iter().any(|l| l == "Generate SQL: DELETE"),
        "{labels:?}"
    );
    let customers = open_table_from_menu(&mut app);
    let t = customers.clone();
    wait(&mut app.vcx, "the customers rows", |vcx| {
        t.read_with(vcx, |t, cx| {
            t.table_state().describing.is_none()
                && matches!(
                    t.results().read(cx).results().tabs().first(),
                    Some(ResultTab::Table(m)) if m.borrow().state() == FetchState::Idle
                        || m.borrow().state().is_final()
                )
        })
    });
    let shown = first_result(&app, &customers).map(|(n, _)| n);
    assert!(shown.is_some_and(|n| n > 0), "{shown:?}");
    // 1 000 customers: more than a page, the read is paused; stop it to free the session.
    app.vcx
        .update(|window, cx| customers.update(cx, |t, cx| t.cancel_run(window, cx)));
    let t = customers.clone();
    wait(&mut app.vcx, "the customers read ends", |vcx| {
        !t.read_with(vcx, |t, _| t.is_running())
    });

    // 3. "Open table data" on a table larger than one page: the read pauses after its first
    // page, the key was read first, so the loaded rows are editable.
    app.reveal(&table_path(engine, &name, &table));
    let tab = open_table_from_menu(&mut app);
    let page = tab.read_with(&app.vcx, |t, _| t.budget().page_rows);
    assert!(page < PAGED_ROWS, "{page}");
    let t = tab.clone();
    wait(&mut app.vcx, "the first page", |vcx| {
        t.read_with(vcx, |t, cx| {
            t.table_state().describing.is_none()
                && matches!(
                    t.results().read(cx).results().tabs().first(),
                    Some(ResultTab::Table(m)) if m.borrow().state() == FetchState::Idle
                )
        })
    });
    assert!(
        tab.read_with(&app.vcx, |t, _| t.is_running()),
        "paused, not ended"
    );
    let loaded = first_result(&app, &tab).map_or(0, |(n, _)| n);
    assert!(loaded > 0 && loaded < PAGED_ROWS as usize, "{loaded}");
    let reason = tab.read_with(&app.vcx, |t, _| t.read_only_reason().map(str::to_owned));
    assert!(
        tab.read_with(&app.vcx, |t, _| t.editing().is_some()),
        "editable: {reason:?}"
    );
    // The toolbar's label follows the declared cancel (I8).
    assert_eq!(
        tab.read_with(&app.vcx, |t, _| t.cancel_label()),
        crate::editor_tab::cancel_label_for(Some(caps.cancel))
    );
    if matches!(engine, Engine::Postgresql | Engine::Mysql | Engine::Mariadb) {
        assert_eq!(tab.read_with(&app.vcx, |t, _| t.cancel_label()), "Cancel");
    }

    // "Edit cell" on the first row's label: offered because the driver declares updates.
    assert!(caps.editing.update, "the drivers declare row updates");
    let ix = tab
        .read_with(&app.vcx, |t, _| t.editing().map(|e| e.tab))
        .unwrap_or_else(|| panic!("editing"));
    let results = tab.read_with(&app.vcx, |t, _| t.results().clone());
    results.update(&mut app.vcx, |r, cx| r.select_cell(ix, 0, 1, cx));
    let (row_id, original) = tab.read_with(&app.vcx, |t, cx| {
        match t.results().read(cx).results().tabs().first() {
            Some(ResultTab::Table(m)) => {
                let m = m.borrow();
                let mut id = String::new();
                m.write_cell_copy(&mut id, 0, 0, "");
                let mut label = String::new();
                m.write_cell_copy(&mut label, 0, 1, "");
                (id, label)
            }
            _ => panic!("a table result"),
        }
    });
    assert_eq!(original, format!("row {row_id}"));
    app.perform(&crate::actions::EditCell);
    assert!(
        app.vcx.update(|window, cx| window.has_active_dialog(cx)),
        "the cell editor opened: {}",
        message(&app, &tab)
    );
    // The dialog's "Apply" sets the cell through the tab (its input is internal to the dialog).
    app.vcx.update(|window, cx| window.close_dialog(cx));
    let edited = format!("edited {row_id}");
    let set = app
        .vcx
        .update(|_, cx| tab.update(cx, |t, cx| t.set_cell(0, 1, Some(&edited), cx)));
    assert_eq!(set, Ok(()));
    assert!(tab.read_with(&app.vcx, |t, _| t.has_pending_edits()));

    // "Save changes" is refused while the result is open, naming the way out.
    app.perform(&crate::actions::SaveChanges);
    let label = tab.read_with(&app.vcx, |t, _| t.cancel_label());
    assert_eq!(
        message(&app, &tab),
        crate::editor_tab::table::save_blocked_by_run_text(label)
    );

    // The toolbar's "Cancel": the run ends, the rows and the edit stay, still editable.
    app.vcx
        .update(|window, cx| tab.update(cx, |t, cx| t.cancel_run(window, cx)));
    let t = tab.clone();
    wait(&mut app.vcx, "the run ends", |vcx| {
        !t.read_with(vcx, |t, _| t.is_running())
    });
    assert_eq!(
        first_result(&app, &tab).map(|(n, _)| n),
        Some(loaded),
        "the loaded rows are kept"
    );
    assert!(tab.read_with(&app.vcx, |t, _| t.has_pending_edits()));
    assert!(
        tab.read_with(&app.vcx, |t, _| t.editing().is_some()),
        "still editable: {:?}",
        tab.read_with(&app.vcx, |t, _| t.read_only_reason().map(str::to_owned))
    );
    assert_eq!(admin.label(&table, &row_id), Some(original.clone()));

    // Now "Save changes" goes through (dev: auto-commit).
    app.perform(&crate::actions::SaveChanges);
    let t = tab.clone();
    wait(&mut app.vcx, "the save", |vcx| {
        t.read_with(vcx, |t, _| {
            !t.table_state().saving && !t.has_pending_edits()
        })
    });
    let said = message(&app, &tab);
    assert!(said.starts_with("Saved 1 changes."), "{said}");
    // Read back through a separate driver session: the engine has the new value.
    assert_eq!(admin.label(&table, &row_id), Some(edited));
    // The refresh after the save pages again: stop it.
    app.vcx
        .update(|window, cx| tab.update(cx, |t, cx| t.cancel_run(window, cx)));
    let t = tab.clone();
    wait(&mut app.vcx, "the refresh ends", |vcx| {
        !t.read_with(vcx, |t, _| t.is_running())
    });
    // Restore the value (the table is dropped afterwards anyway).
    admin.ok(&format!(
        "UPDATE {table} SET label = '{original}' WHERE id = {row_id}"
    ));
    assert_eq!(admin.label(&table, &row_id), Some(original));

    app.disconnect(id);
    app.assert_no_password_on_disk();
}

// ---------------------------------------------------------------------------------------------
// Scenario 4: "Export table…" to CSV and "Import CSV…" into a scratch table.

fn export_and_import_csv(cx: &mut TestAppContext, engine: Engine) {
    let Some((mut app, tmp)) = start(cx, engine) else {
        return;
    };
    let mut admin = Admin::connect(&app.target);
    let target_table = admin.scratch_table("import", 0);
    let name = format!("Transfer {}", engine.id());
    let id = app.create_connection(&name, ConnectionTypeId::dev(), false);
    app.connect(id);

    // Export the nine dataset's `customers` to a new CSV file.
    let out = tmp.path().join("out");
    std::fs::create_dir_all(&out).unwrap_or_else(|e| panic!("{e}"));
    let file = out.join("customers.csv");
    app.reveal(&table_path(engine, &name, "customers"));
    crate::transfer::OPENED_EXPORT.with(|o| o.borrow_mut().take());
    app.perform(&crate::actions::ExportTable);
    let dialog = crate::transfer::OPENED_EXPORT
        .with(|o| o.borrow().clone())
        .unwrap_or_else(|| panic!("the export dialog"));
    let input = dialog.read_with(&app.vcx, |d, _| d.path_input().clone());
    let path = file.to_string_lossy().into_owned();
    app.vcx
        .update(|window, cx| input.update(cx, |s, cx| s.set_value(path, window, cx)));
    dialog.update_in(&mut app.vcx, |d, window, cx| d.start(window, cx));
    let d = dialog.clone();
    wait(&mut app.vcx, "the export", |vcx| {
        d.read_with(vcx, |d, _| {
            !d.is_busy()
                && !matches!(
                    d.stage(),
                    crate::transfer::export::ExportStage::Editing
                        | crate::transfer::export::ExportStage::Checking
                )
        })
    });
    let rows = dialog.read_with(&app.vcx, |d, _| match d.stage() {
        crate::transfer::export::ExportStage::Done(s) => s.rows,
        other => panic!("not done: {other:?}"),
    });
    let expected = admin.count("customers");
    assert_eq!(i64::try_from(rows).ok(), Some(expected));
    let text = std::fs::read_to_string(&file).unwrap_or_else(|e| panic!("{e}"));
    assert!(text.lines().count() > usize::try_from(expected).unwrap_or(0));
    app.vcx.update(|window, cx| window.close_dialog(cx));

    // Import a small CSV into the scratch table.
    let csv = tmp.path().join("in.csv");
    std::fs::write(&csv, "id,label\n1,one\n2,two\n3,three\n4,four\n5,five\n")
        .unwrap_or_else(|e| panic!("{e}"));
    app.reveal(&table_path(engine, &name, &target_table));
    crate::transfer::OPENED_IMPORT.with(|o| o.borrow_mut().take());
    app.perform(&crate::actions::ImportCsv);
    let mut dialog = None;
    wait(&mut app.vcx, "the import dialog", |_| {
        dialog = crate::transfer::OPENED_IMPORT.with(|o| o.borrow().clone());
        dialog.is_some()
    });
    let dialog = dialog.unwrap_or_else(|| panic!("the import dialog"));
    let input = dialog.read_with(&app.vcx, |d, _| d.path_input().clone());
    let path = csv.to_string_lossy().into_owned();
    app.vcx
        .update(|window, cx| input.update(cx, |s, cx| s.set_value(path, window, cx)));
    dialog.update_in(&mut app.vcx, |d, window, cx| d.read_preview(window, cx));
    let d = dialog.clone();
    wait(&mut app.vcx, "the CSV preview", |vcx| {
        d.read_with(vcx, |d, _| {
            matches!(d.stage(), crate::transfer::import::ImportStage::Preview)
                && d.target().is_some()
        })
    });
    assert_eq!(
        dialog.read_with(&app.vcx, |d, _| d.mapping().to_vec()),
        vec![Some(0), Some(1)],
        "by header name"
    );
    dialog.update_in(&mut app.vcx, |d, window, cx| d.start(window, cx));
    let d = dialog.clone();
    wait(&mut app.vcx, "the import", |vcx| {
        d.read_with(vcx, |d, _| {
            matches!(
                d.stage(),
                crate::transfer::import::ImportStage::Done(_)
                    | crate::transfer::import::ImportStage::Failed(_)
            )
        })
    });
    let (end, inserted) = dialog.read_with(&app.vcx, |d, _| match d.stage() {
        crate::transfer::import::ImportStage::Done(s) => (s.end, s.rows_inserted),
        other => panic!("not done: {other:?}"),
    });
    assert_eq!(end, ninesql_core::ImportEnd::Completed);
    assert_eq!(inserted, 5);
    assert_eq!(admin.count(&target_table), 5);
    assert_eq!(admin.label(&target_table, "3").as_deref(), Some("three"));
    app.vcx.update(|window, cx| window.close_dialog(cx));

    app.disconnect(id);
    app.assert_no_password_on_disk();
}

// ---------------------------------------------------------------------------------------------
// Scenario 5: a read-only connection refuses writes engine-side, and the UI says so.

fn read_only_refuses_writes(cx: &mut TestAppContext, engine: Engine) {
    let Some((mut app, _tmp)) = start(cx, engine) else {
        return;
    };
    let mut admin = Admin::connect(&app.target);
    let table = admin.scratch_table("ro", 3);
    let name = format!("Read-only {}", engine.id());
    let id = app.create_connection(&name, ConnectionTypeId::dev(), true);
    let editor_tab = app.connect(id);
    let session = app
        .hub
        .read_with(&app.vcx, |h, _| h.sessions(id).first().cloned())
        .unwrap_or_else(|| panic!("a session"));
    assert!(
        session.info().read_only,
        "the driver opened a read-only session"
    );

    // A write statement reaches the engine, which refuses it; the editor shows the error.
    execute(
        &mut app,
        &editor_tab,
        &format!("INSERT INTO {table} (id, label) VALUES (100, 'written')"),
    );
    let t = editor_tab.clone();
    wait(&mut app.vcx, "the refused write", |vcx| {
        !t.read_with(vcx, |t, _| t.is_running())
    });
    let editor = editor_tab.read_with(&app.vcx, |t, _| t.editor().clone());
    let shown = editor.read_with(&app.vcx, |e, _| e.error().map(|e| e.message.clone()));
    assert!(
        shown.is_some(),
        "the refusal is shown: {}",
        message(&app, &editor_tab)
    );
    assert_eq!(admin.count(&table), 3, "nothing was written");

    // The navigator offers reading only (no write template), and the table opens read-only.
    app.reveal(&table_path(engine, &name, &table));
    let labels: Vec<String> = app
        .nav
        .read_with(&app.vcx, |n, cx| n.selected_object_menu(cx))
        .into_iter()
        .flatten()
        .map(|(l, _)| l)
        .collect();
    assert_eq!(labels.first().map(String::as_str), Some("Open table data"));
    for write in [
        "Generate SQL: INSERT",
        "Generate SQL: UPDATE",
        "Generate SQL: DELETE",
    ] {
        assert!(!labels.iter().any(|l| l == write), "{labels:?}");
    }
    // "Import CSV…" is refused on a read-only session: no dialog.
    crate::transfer::OPENED_IMPORT.with(|o| o.borrow_mut().take());
    app.perform(&crate::actions::ImportCsv);
    app.vcx.run_until_parked();
    assert!(crate::transfer::OPENED_IMPORT.with(|o| o.borrow().is_none()));
    let tab = open_table_from_menu(&mut app);
    let t = tab.clone();
    wait(&mut app.vcx, "the read-only table", |vcx| {
        t.read_with(vcx, |t, cx| {
            !t.is_running()
                && t.table_state().describing.is_none()
                && t.results().read(cx).tab_of_statement(0).is_some()
        })
    });
    assert_eq!(first_result(&app, &tab).map(|(n, _)| n), Some(3));
    assert!(tab.read_with(&app.vcx, |t, _| t.editing().is_none()));
    let reason = tab
        .read_with(&app.vcx, |t, _| t.read_only_reason().map(str::to_owned))
        .unwrap_or_default();
    assert!(!reason.is_empty());
    // "Edit cell" is refused with the reason.
    app.perform(&crate::actions::EditCell);
    assert!(!app.vcx.update(|window, cx| window.has_active_dialog(cx)));

    app.disconnect(id);
    assert_eq!(admin.count(&table), 3);
    app.assert_no_password_on_disk();
}

// ---------------------------------------------------------------------------------------------
// Scenario 6: on a connection typed production, a `DELETE` without `WHERE` asks first, and
// declining runs nothing.

fn production_delete_is_confirmed(cx: &mut TestAppContext, engine: Engine) {
    let Some((mut app, _tmp)) = start(cx, engine) else {
        return;
    };
    let mut admin = Admin::connect(&app.target);
    let table = admin.scratch_table("prod", 3);
    let name = format!("Production {}", engine.id());
    let id = app.create_connection(&name, ConnectionTypeId::prod(), false);
    let tab = app.connect(id);
    assert!(
        app.ws
            .read_with(&app.vcx, |ws, cx| ws.frame_color(cx).is_some()),
        "production frame"
    );
    let session = app
        .hub
        .read_with(&app.vcx, |h, _| h.sessions(id).first().cloned())
        .unwrap_or_else(|| panic!("a session"));
    // Production is manual-commit.
    assert!(!session.transaction_state().auto_commit, "manual commit");
    execute(&mut app, &tab, &format!("DELETE FROM {table}"));
    let t = tab.clone();
    wait(&mut app.vcx, "the confirmation", |vcx| {
        t.read_with(vcx, |t, _| t.awaiting_confirmation())
    });
    assert!(app.vcx.update(|window, cx| window.has_active_dialog(cx)));
    assert_eq!(
        tab.read_with(&app.vcx, |t, _| t.pending_statement().map(str::to_owned)),
        Some(format!("DELETE FROM {table}"))
    );
    // "Cancel" in the dialog.
    app.vcx.update(|window, cx| {
        window.close_dialog(cx);
        tab.update(cx, |t, cx| t.decline(window, cx));
    });
    app.vcx.run_until_parked();
    assert!(!tab.read_with(&app.vcx, |t, _| t.is_running()));
    let said = message(&app, &tab);
    assert!(said.contains("not confirmed"), "{said}");
    // No transaction was opened: the metadata reads the editor makes while typing end the
    // transaction they start with `autocommit = 0` on the MySQL family (mysqlwire).
    assert!(!session.transaction_state().in_transaction, "nothing ran");
    assert!(update_counts(&app, &tab).is_empty(), "declined: not run");
    assert_eq!(admin.count(&table), 3, "nothing was deleted");

    app.disconnect(id);
    app.assert_no_password_on_disk();
}

macro_rules! engine_tests {
    ($($engine:ident => $variant:ident),* $(,)?) => {
        $(
            mod $engine {
                use super::*;

                #[gpui::test]
                fn connect_browse_and_edit_a_paged_table(cx: &mut TestAppContext) {
                    connect_browse_edit_paged(cx, Engine::$variant);
                }

                #[gpui::test]
                fn export_and_import_csv_through_the_dialogs(cx: &mut TestAppContext) {
                    export_and_import_csv(cx, Engine::$variant);
                }

                #[gpui::test]
                fn a_read_only_connection_refuses_writes_engine_side(cx: &mut TestAppContext) {
                    read_only_refuses_writes(cx, Engine::$variant);
                }

                #[gpui::test]
                fn a_production_delete_asks_and_declining_runs_nothing(cx: &mut TestAppContext) {
                    production_delete_is_confirmed(cx, Engine::$variant);
                }
            }
        )*
    };
}

engine_tests!(
    postgresql => Postgresql,
    mysql => Mysql,
    mariadb => Mariadb,
    sqlite => Sqlite,
);

/// The navigator expanded right after "Connect", while the editor's completion warm-up still
/// loads metadata: both loads go to the connection's metadata session and one of them failed
/// with "session busy: a result is still streaming" (`DriverError::Busy`), shown as an error
/// row instead of the tables (seen on PostgreSQL, MySQL and MariaDB in the first runs of these
/// tests; fixed in the core by eb3d230, metadata loads serialised per session). The other tests
/// still wait for the warm-up first ([`App::settle`]), as a user looking at the window would.
#[gpui::test]
fn navigator_right_after_connect_loads_without_a_busy_session(cx: &mut TestAppContext) {
    let Some((mut app, _tmp)) = start(cx, Engine::Postgresql) else {
        return;
    };
    let name = "Busy postgresql";
    let id = app.create_connection(name, ConnectionTypeId::dev(), false);
    app.connect_unsettled(id);
    app.reveal(&table_path(Engine::Postgresql, name, "customers"));
    assert!(
        !app.rows().iter().any(|r| r.contains("session busy")),
        "{:?}",
        app.rows()
    );
    app.disconnect(id);
    app.assert_no_password_on_disk();
}
