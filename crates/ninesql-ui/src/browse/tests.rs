//! `TestAppContext` tests of metadata browsing against the core's mock driver (catalog `nine`
//! → schema `public` → 50 tables `t000`… → columns `id`, `label`; DDL for tables): the lazy
//! tree with loading rows, cancel, "Load more", refresh and invalidation; filters; "Find object"
//! and reveal; the object editor; "Generate SQL"; editor completion; keyboard navigation.

use gpui::{Entity, TestAppContext, VisualTestContext};
use gpui_kit::component::WindowExt as _;
use ninesql_core::driver_api::ObjectKind;
use ninesql_core::driver_api::metadata::ObjectPath;
use ninesql_core::{Auth, ConnectionId, ConnectionTypeId, MetadataConfig, NodeKey};

use super::generate::{Template, TemplateAllowance};
use super::object_editor::ObjectTab;
use super::search::SearchScope;
use crate::connections::connect_flow::Purpose;
use crate::connections::hub::ConnectionHub;
use crate::connections::tests::{mock_connection, mock_hub_with_metadata, open_live, wait_until};
use crate::navigator::Navigator;
use crate::workspace::Workspace;

struct Fixture {
    ws: Entity<Workspace>,
    hub: Entity<ConnectionHub>,
    nav: Entity<Navigator>,
    id: ConnectionId,
    vcx: VisualTestContext,
    _tmp: tempfile::TempDir,
}

fn small_pages() -> MetadataConfig {
    let mut c = MetadataConfig::default();
    c.page_size = 20;
    c.max_pages_per_load = 1;
    c
}

/// A window on the mock driver with one connection, connected.
fn connected(cx: &mut TestAppContext, metadata: MetadataConfig) -> Fixture {
    connected_with(cx, metadata, false)
}

/// [`connected`], on a read-only connection when `read_only`.
fn connected_with(cx: &mut TestAppContext, metadata: MetadataConfig, read_only: bool) -> Fixture {
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let mut c = mock_connection("Live", ConnectionTypeId::dev(), Auth::None);
    if read_only {
        c.read_only = Some(true);
    }
    let id = c.id;
    let hub = mock_hub_with_metadata(cx, tmp.path(), vec![c], metadata);
    let (ws, mut vcx) = open_live(cx);
    ws.update_in(&mut vcx, |ws, window, cx| {
        ws.connect(id, Purpose::Open, window, cx);
    });
    wait_until(&mut vcx, |vcx| hub.read_with(vcx, |h, _| h.is_open(id)));
    let nav = ws.read_with(&vcx, |ws, _| ws.navigator().clone());
    Fixture {
        ws,
        hub,
        nav,
        id,
        vcx,
        _tmp: tmp,
    }
}

fn labels(f: &Fixture) -> Vec<String> {
    f.nav
        .read_with(&f.vcx, |n, cx| n.rows(cx))
        .into_iter()
        .map(|(_, l)| l)
        .collect()
}

fn index_of(f: &Fixture, label: &str) -> usize {
    labels(f)
        .iter()
        .position(|l| l == label)
        .unwrap_or_else(|| panic!("no row {label:?} in {:?}", labels(f)))
}

fn expand(f: &mut Fixture, label: &str) {
    let ix = index_of(f, label);
    f.nav.update(&mut f.vcx, |n, cx| n.expand(ix, cx));
}

fn wait_for(f: &mut Fixture, label: &str) {
    let nav = f.nav.clone();
    let label = label.to_owned();
    wait_until(&mut f.vcx, |vcx| {
        nav.read_with(vcx, |n, cx| n.rows(cx).iter().any(|(_, l)| *l == label))
    });
}

fn path_of(table: &str) -> ObjectPath {
    ObjectPath::root()
        .child(ObjectKind::Catalog, "nine")
        .child(ObjectKind::Schema, "public")
        .child(ObjectKind::Table, table)
}

fn metadata_of(f: &Fixture) -> ninesql_core::MetadataService {
    let metadata = f.hub.read_with(&f.vcx, |h, _| h.metadata());
    metadata.unwrap_or_else(|| panic!("metadata service"))
}

/// Waits for the editor tab's completion warm-up to end, then empties the metadata cache, so
/// the tree's loads are real loads.
fn settle(f: &mut Fixture) {
    let metadata = metadata_of(f);
    let id = f.id;
    let tab = f.ws.read_with(&f.vcx, |ws, _| ws.active_editor().cloned());
    if let Some(tab) = tab {
        wait_until(&mut f.vcx, |vcx| {
            tab.read_with(vcx, |t, _| t.default_schema().is_some())
        });
    }
    let mut quiet = 0;
    wait_until(&mut f.vcx, |_| {
        if metadata.cache_stats(id).is_none_or(|s| s.in_flight == 0) {
            quiet += 1;
        } else {
            quiet = 0;
        }
        quiet >= 5
    });
    metadata.invalidate(id, &ObjectPath::root());
    f.vcx.run_until_parked();
}

/// Expands Live → Databases → nine → Schemas → public → Tables (single kinds auto-expand).
fn open_tables(f: &mut Fixture) {
    expand(f, "Live");
    wait_for(f, "nine");
    expand(f, "nine");
    wait_for(f, "public");
    expand(f, "public");
    wait_for(f, "t000");
}

#[gpui::test]
fn expanding_loads_lazily_with_a_loading_row_and_declared_kinds_only(cx: &mut TestAppContext) {
    let mut f = connected(cx, MetadataConfig::default());
    settle(&mut f);
    assert_eq!(
        labels(&f),
        vec!["Live"],
        "nothing is loaded before expanding"
    );
    expand(&mut f, "Live");
    // The connection's single root kind ("Databases") expands at once and shows its load.
    assert_eq!(labels(&f), vec!["Live", "Databases", "Loading…"]);
    wait_for(&mut f, "nine");
    expand(&mut f, "nine");
    wait_for(&mut f, "public");
    expand(&mut f, "public");
    wait_for(&mut f, "t049");
    let rows = labels(&f);
    assert_eq!(
        &rows[..7],
        &[
            "Live",
            "Databases",
            "nine",
            "Schemas",
            "public",
            "Tables",
            "t000"
        ]
    );
    assert_eq!(rows.len(), 6 + 50, "{rows:?}");
    // Only the kinds the mock declares (I8): a table lists columns, nothing else.
    expand(&mut f, "t001");
    wait_for(&mut f, "label");
    let rows = labels(&f);
    let t001 = index_of(&f, "t001");
    assert_eq!(&rows[t001 + 1..t001 + 4], &["Columns", "id", "label"]);
    let depths = f.nav.read_with(&f.vcx, |n, cx| n.rows(cx));
    assert_eq!(depths[t001 + 1].0, depths[t001].0 + 1);
    assert_eq!(f.nav.read_with(&f.vcx, |n, _| n.loads_in_flight()), 0);
}

#[gpui::test]
fn collapsing_a_loading_group_cancels_its_request(cx: &mut TestAppContext) {
    let mut f = connected(cx, MetadataConfig::default());
    settle(&mut f);
    expand(&mut f, "Live");
    let key = NodeKey::new(ObjectPath::root(), ObjectKind::Catalog);
    let id = f.id;
    assert!(f.nav.read_with(&f.vcx, |n, _| n.is_loading(id, &key)));
    let ix = index_of(&f, "Databases");
    f.nav.update(&mut f.vcx, |n, cx| n.collapse(ix, cx));
    assert!(
        !f.nav.read_with(&f.vcx, |n, _| n.is_loading(id, &key)),
        "collapse dropped the request"
    );
    assert_eq!(labels(&f), vec!["Live", "Databases"]);
    let metadata = metadata_of(&f);
    wait_until(&mut f.vcx, |_| {
        metadata.cache_stats(id).is_none_or(|s| s.in_flight == 0)
    });
    // Expanding again loads it.
    expand(&mut f, "Databases");
    wait_for(&mut f, "nine");
}

#[gpui::test]
fn incomplete_groups_offer_load_more(cx: &mut TestAppContext) {
    let mut f = connected(cx, small_pages());
    settle(&mut f);
    open_tables(&mut f);
    let rows = labels(&f);
    assert!(rows.contains(&"t019".to_owned()));
    assert!(!rows.contains(&"t020".to_owned()));
    assert_eq!(rows.last().map(String::as_str), Some("Load more"));
    // Enter on "Load more".
    let ix = index_of(&f, "Load more");
    f.nav.update(&mut f.vcx, |n, cx| n.activate(ix, cx));
    wait_for(&mut f, "t039");
    // The action on the selected group.
    let ix = index_of(&f, "Tables");
    f.nav
        .update(&mut f.vcx, |n, cx| n.select_index(Some(ix), cx));
    f.ws.update_in(&mut f.vcx, |ws, window, cx| {
        ws.load_more_objects(window, cx)
    });
    wait_for(&mut f, "t049");
    assert!(!labels(&f).contains(&"Load more".to_owned()), "complete");
}

#[gpui::test]
fn refresh_reloads_and_a_closed_session_clears_the_tree(cx: &mut TestAppContext) {
    let mut f = connected(cx, MetadataConfig::default());
    open_tables(&mut f);
    let ix = index_of(&f, "t003");
    f.nav
        .update(&mut f.vcx, |n, cx| n.select_index(Some(ix), cx));
    let done = f.nav.update(&mut f.vcx, |n, cx| n.refresh_selected(cx));
    assert!(done);
    assert!(
        labels(&f).contains(&"Loading…".to_owned()),
        "refresh shows its load"
    );
    wait_for(&mut f, "t003");
    let id = f.id;
    f.ws.update(&mut f.vcx, |ws, cx| ws.disconnect_now(id, cx));
    let nav = f.nav.clone();
    wait_until(&mut f.vcx, |vcx| {
        nav.read_with(vcx, |n, cx| {
            n.rows(cx)
                .iter()
                .map(|(_, l)| l.as_str())
                .collect::<Vec<_>>()
                == vec!["Live", "Not connected"]
        })
    });
}

#[gpui::test]
fn filters_are_validated_saved_and_reload_the_tree(cx: &mut TestAppContext) {
    let mut f = connected(cx, MetadataConfig::default());
    open_tables(&mut f);
    let ix = index_of(&f, "Live");
    f.nav
        .update(&mut f.vcx, |n, cx| n.select_index(Some(ix), cx));
    f.ws.update_in(&mut f.vcx, |ws, window, cx| {
        ws.configure_navigator_filters(window, cx);
    });
    assert!(f.vcx.update(|window, cx| window.has_active_dialog(cx)));
    f.vcx.update(|window, cx| window.close_dialog(cx));
    let metadata = metadata_of(&f);
    let id = f.id;
    let dialog = f.vcx.update(|window, cx| {
        super::filters::open_filter_dialog(
            metadata.clone(),
            id,
            "Live".into(),
            vec![ObjectKind::Catalog, ObjectKind::Schema, ObjectKind::Table],
            window,
            cx,
        )
    });
    assert_eq!(
        dialog.read_with(&f.vcx, |d, _| d.kinds()),
        vec![ObjectKind::Catalog, ObjectKind::Schema, ObjectKind::Table]
    );
    // A control character is refused, with the error on its kind.
    f.vcx.update(|window, cx| {
        dialog.update(cx, |d, cx| {
            d.set_patterns(ObjectKind::Table, "", "t00*, a\tb", window, cx);
            d.save(window, cx);
        });
    });
    let error = dialog.read_with(&f.vcx, |d, _| {
        d.kind_error(ObjectKind::Table).map(str::to_owned)
    });
    assert!(
        error
            .as_deref()
            .is_some_and(|e| e.contains("control character")),
        "{error:?}"
    );
    assert!(metadata.filters(id).is_empty(), "nothing saved");
    f.vcx.update(|window, cx| {
        dialog.update(cx, |d, cx| {
            d.set_patterns(ObjectKind::Table, "", "t00*", window, cx);
            d.save(window, cx);
        });
    });
    let d = dialog.clone();
    wait_until(&mut f.vcx, |vcx| d.read_with(vcx, |d, _| d.saved()));
    wait_for(&mut f, "10 hidden by filter");
    let rows = labels(&f);
    assert!(!rows.contains(&"t000".to_owned()));
    assert!(rows.contains(&"t010".to_owned()));
}

#[gpui::test]
fn find_object_searches_loaded_metadata_and_reveals_lazily(cx: &mut TestAppContext) {
    let mut f = connected(cx, MetadataConfig::default());
    open_tables(&mut f);
    let metadata = metadata_of(&f);
    let id = f.id;
    let chosen = std::rc::Rc::new(std::cell::RefCell::new(None));
    let sink = chosen.clone();
    let dialog = f.vcx.update(|window, cx| {
        super::search::open_find_object(
            metadata.clone(),
            vec![SearchScope {
                id,
                name: "Live".into(),
                selected: true,
            }],
            std::rc::Rc::new(move |choice, _, _| {
                *sink.borrow_mut() = Some(choice);
            }),
            window,
            cx,
        )
    });
    let input = dialog.read_with(&f.vcx, |d, _| d.query().clone());
    f.vcx.update(|window, cx| {
        input.update(cx, |s, cx| s.set_value("t042", window, cx));
        // `set_value` emits no change event (typing does).
        dialog.update(cx, |d, cx| d.search(cx));
    });
    let d = dialog.clone();
    wait_until(&mut f.vcx, |vcx| {
        d.read_with(vcx, |d, _| d.results().is_some_and(|r| !r.hits.is_empty()))
    });
    let hit = dialog.read_with(&f.vcx, |d, _| d.selected_hit().cloned());
    let Some(hit) = hit else {
        panic!("a hit");
    };
    assert_eq!(hit.path, path_of("t042"));
    assert_eq!(hit.connection, id);
    f.vcx
        .update(|window, cx| dialog.update(cx, |d, cx| d.choose(false, window, cx)));
    assert!(matches!(
        chosen.borrow().clone(),
        Some(super::search::FindChoice::Reveal(c, _)) if c == id
    ));

    // Reveal from a collapsed tree: the path is expanded lazily, then the row is selected.
    let ix = index_of(&f, "Live");
    f.nav.update(&mut f.vcx, |n, cx| n.collapse(ix, cx));
    assert_eq!(labels(&f), vec!["Live"]);
    f.ws.update_in(&mut f.vcx, |ws, window, cx| {
        ws.reveal_object(id, path_of("t042"), window, cx);
    });
    let nav = f.nav.clone();
    wait_until(&mut f.vcx, |vcx| {
        !nav.read_with(vcx, |n, _| n.is_revealing())
    });
    let selected = f.nav.read_with(&f.vcx, |n, _| n.selection());
    match selected {
        Some(crate::navigator::Selection::Object(c, info)) => {
            assert_eq!(c, id);
            assert_eq!(info.path, path_of("t042"));
        }
        other => panic!("selected {other:?}"),
    }
    // An object that is not there is reported, not invented.
    f.ws.update_in(&mut f.vcx, |ws, window, cx| {
        ws.reveal_object(id, path_of("nope"), window, cx);
    });
    let nav = f.nav.clone();
    wait_until(&mut f.vcx, |vcx| {
        !nav.read_with(vcx, |n, _| n.is_revealing())
    });
}

#[gpui::test]
fn find_object_action_opens_the_dialog(cx: &mut TestAppContext) {
    let mut f = connected(cx, MetadataConfig::default());
    f.ws.update_in(&mut f.vcx, |ws, window, cx| {
        let handled = ws.perform(&crate::actions::FindObject, window, cx);
        assert!(handled);
    });
    assert!(f.vcx.update(|window, cx| window.has_active_dialog(cx)));
}

#[gpui::test]
fn the_object_editor_shows_what_the_metadata_provides(cx: &mut TestAppContext) {
    let mut f = connected(cx, MetadataConfig::default());
    open_tables(&mut f);
    let ix = index_of(&f, "t001");
    f.nav
        .update(&mut f.vcx, |n, cx| n.select_index(Some(ix), cx));
    f.ws.update_in(&mut f.vcx, |ws, window, cx| {
        ws.perform(&crate::actions::OpenObjectEditor, window, cx);
    });
    let editor =
        f.ws.read_with(&f.vcx, |ws, _| ws.active_object_editor().cloned());
    let Some(editor) = editor else {
        panic!("an object editor tab");
    };
    assert_eq!(
        f.ws.read_with(&f.vcx, |ws, cx| ws.active_title(cx)),
        Some("t001".into())
    );
    // The mock declares DDL for tables (I8): the DDL tab is there.
    assert_eq!(
        editor.read_with(&f.vcx, |e, _| e.tabs()),
        vec![
            ObjectTab::Properties,
            ObjectTab::Columns,
            ObjectTab::Keys,
            ObjectTab::Indexes,
            ObjectTab::Ddl
        ]
    );
    assert!(editor.read_with(&f.vcx, |e, _| e.info().is_some()));
    // Lists the mock does not provide say so.
    assert!(!editor.read_with(&f.vcx, |e, _| e.available(ObjectTab::Indexes)));
    assert!(!editor.read_with(&f.vcx, |e, _| e.available(ObjectTab::Keys)));
    f.vcx.update(|window, cx| {
        editor.update(cx, |e, cx| e.activate(ObjectTab::Columns, window, cx));
    });
    let e = editor.clone();
    wait_until(&mut f.vcx, |vcx| {
        e.read_with(vcx, |e, _| {
            e.section(ObjectKind::Column)
                .and_then(|s| s.children.as_ref())
                .is_some_and(|c| c.items.len() == 2)
        })
    });
    f.vcx.update(|window, cx| {
        editor.update(cx, |e, cx| e.activate(ObjectTab::Ddl, window, cx));
    });
    let e = editor.clone();
    wait_until(&mut f.vcx, |vcx| {
        e.read_with(vcx, |e, _| e.ddl_text().is_some())
    });
    let ddl = editor.read_with(&f.vcx, |e, _| e.ddl_text().map(str::to_owned));
    assert!(ddl.is_some_and(|d| d.starts_with("CREATE TABLE \"t001\"")));
    // Refresh reloads what is shown (no list or DDL left waiting).
    f.vcx
        .update(|window, cx| editor.update(cx, |e, cx| e.refresh(window, cx)));
    let e = editor.clone();
    wait_until(&mut f.vcx, |vcx| {
        e.read_with(vcx, |e, _| e.ddl_text().is_some())
    });
    f.vcx.update(|window, cx| {
        editor.update(cx, |e, cx| e.activate(ObjectTab::Columns, window, cx));
    });
    let e = editor.clone();
    wait_until(&mut f.vcx, |vcx| {
        e.read_with(vcx, |e, _| {
            e.section(ObjectKind::Column)
                .and_then(|s| s.children.as_ref())
                .is_some_and(|c| c.items.len() == 2)
        })
    });
    f.vcx.update(|window, cx| {
        editor.update(cx, |e, cx| e.activate(ObjectTab::Ddl, window, cx));
    });
    // "Copy DDL" on the active object editor.
    f.ws.update_in(&mut f.vcx, |ws, window, cx| ws.copy_object_ddl(window, cx));
    let copied = f.vcx.read_from_clipboard().and_then(|c| c.text());
    assert!(copied.is_some_and(|t| t.contains("CREATE TABLE")));
    // Opening it again activates the same tab.
    let tabs = f.ws.read_with(&f.vcx, |ws, cx| ws.tab_titles(cx).len());
    let id = f.id;
    f.ws.update_in(&mut f.vcx, |ws, window, cx| {
        ws.open_object_editor(id, path_of("t001"), None, window, cx);
    });
    assert_eq!(
        f.ws.read_with(&f.vcx, |ws, cx| ws.tab_titles(cx).len()),
        tabs
    );
    // Closing it drops its loads.
    f.ws.update_in(&mut f.vcx, |ws, window, cx| ws.close_active_tab(window, cx));
    assert!(f.ws.read_with(&f.vcx, |ws, _| ws.active_object_editor().is_none()));
}

#[gpui::test]
fn generate_sql_opens_quoted_templates_that_never_run(cx: &mut TestAppContext) {
    let mut f = connected(cx, MetadataConfig::default());
    open_tables(&mut f);
    let ix = index_of(&f, "t000");
    f.nav
        .update(&mut f.vcx, |n, cx| n.select_index(Some(ix), cx));
    assert!(f.ws.read_with(&f.vcx, |ws, cx| ws.selection_is_table_like(cx)));
    f.ws.update_in(&mut f.vcx, |ws, window, cx| {
        ws.perform(&crate::actions::GenerateDeleteSql, window, cx);
    });
    let ws = f.ws.clone();
    wait_until(&mut f.vcx, |vcx| {
        ws.read_with(vcx, |ws, cx| {
            ws.active_title(cx)
                .is_some_and(|t| t.as_ref() == "DELETE t000")
        })
    });
    let tab = f.ws.read_with(&f.vcx, |ws, _| ws.active_editor().cloned());
    let Some(tab) = tab else {
        panic!("an SQL editor");
    };
    let text = tab.read_with(&f.vcx, |t, cx| t.editor().read(cx).text(cx));
    assert_eq!(
        text, "DELETE FROM \"public\".\"t000\"\nWHERE \"id\" = <id>;\n",
        "quoted with the session dialect, a WHERE placeholder"
    );
    let id = f.id;
    assert_eq!(tab.read_with(&f.vcx, |t, _| t.connection()), Some(id));
    assert!(!tab.read_with(&f.vcx, |t, _| t.is_running()), "never run");

    f.ws.update_in(&mut f.vcx, |ws, window, cx| {
        ws.generate(id, path_of("t000"), Template::Update, window, cx);
    });
    let ws = f.ws.clone();
    wait_until(&mut f.vcx, |vcx| {
        ws.read_with(vcx, |ws, cx| {
            ws.active_title(cx)
                .is_some_and(|t| t.as_ref() == "UPDATE t000")
        })
    });
    let tab = f.ws.read_with(&f.vcx, |ws, _| ws.active_editor().cloned());
    let text = tab.map(|t| t.read_with(&f.vcx, |t, cx| t.editor().read(cx).text(cx)));
    assert_eq!(
        text.as_deref(),
        Some("UPDATE \"public\".\"t000\"\nSET\n  \"label\" = <label>\nWHERE \"id\" = <id>;\n")
    );

    // "Show DDL" opens the DDL in a new editor (the navigator still selects t000).
    f.ws.update_in(&mut f.vcx, |ws, window, cx| {
        ws.perform(&crate::actions::ShowObjectDdl, window, cx);
    });
    let ws = f.ws.clone();
    wait_until(&mut f.vcx, |vcx| {
        ws.read_with(vcx, |ws, cx| {
            ws.active_title(cx)
                .is_some_and(|t| t.as_ref() == "DDL t000")
        })
    });
}

#[gpui::test]
fn editor_completion_reads_the_metadata_cache(cx: &mut TestAppContext) {
    let mut f = connected(cx, MetadataConfig::default());
    let tab = f.ws.read_with(&f.vcx, |ws, _| ws.active_editor().cloned());
    let Some(tab) = tab else {
        panic!("an editor tab");
    };
    let id = f.id;
    wait_until(&mut f.vcx, |vcx| {
        tab.read_with(vcx, |t, _| t.connection() == Some(id))
    });
    // The default schema is the first one the driver reports, then warmed.
    wait_until(&mut f.vcx, |vcx| {
        tab.read_with(vcx, |t, _| t.default_schema().as_deref() == Some("public"))
    });
    let provider = tab.read_with(&f.vcx, |t, cx| t.editor().read(cx).setup().metadata.clone());
    wait_until(&mut f.vcx, |_| provider.objects(None).len() == 50);
    assert_eq!(provider.schemas(), vec!["public".to_owned()]);
    assert_eq!(provider.default_schema().as_deref(), Some("public"));
    wait_until(&mut f.vcx, |_| provider.columns(None, "t000").is_some());
    let cols = provider.columns(None, "t000").unwrap_or_default();
    assert_eq!(cols[0].name, "id");
    assert_eq!(cols[0].data_type.as_deref(), Some("bigint"));
}

#[gpui::test]
fn the_tree_is_driven_by_the_keyboard(cx: &mut TestAppContext) {
    let mut f = connected(cx, MetadataConfig::default());
    f.ws.update_in(&mut f.vcx, |ws, window, cx| ws.focus_navigator(window, cx));
    assert_eq!(f.nav.read_with(&f.vcx, |n, _| n.selected_index()), Some(0));
    f.vcx.simulate_keystrokes("right");
    wait_for(&mut f, "nine");
    f.vcx.simulate_keystrokes("down down");
    assert_eq!(f.nav.read_with(&f.vcx, |n, _| n.selected_index()), Some(2));
    f.vcx.simulate_keystrokes("enter");
    wait_for(&mut f, "public");
    f.vcx.simulate_keystrokes("left");
    assert_eq!(labels(&f), vec!["Live", "Databases", "nine"]);
    f.vcx.simulate_keystrokes("left");
    assert_eq!(
        f.nav.read_with(&f.vcx, |n, _| n.selected_index()),
        Some(1),
        "left on a collapsed row goes to its parent"
    );
    f.vcx.simulate_keystrokes("home");
    assert_eq!(f.nav.read_with(&f.vcx, |n, _| n.selected_index()), Some(0));
    f.vcx.simulate_keystrokes("end");
    assert_eq!(f.nav.read_with(&f.vcx, |n, _| n.selected_index()), Some(2));
}

#[gpui::test]
fn navigator_actions_are_bound_and_in_the_palette(_cx: &mut TestAppContext) {
    for name in [
        "ninesql::RefreshNavigatorNode",
        "ninesql::LoadMoreObjects",
        "ninesql::ConfigureNavigatorFilters",
        "ninesql::FindObject",
        "ninesql::OpenObjectEditor",
        "ninesql::GenerateSelectSql",
        "ninesql::GenerateInsertSql",
        "ninesql::GenerateUpdateSql",
        "ninesql::GenerateDeleteSql",
        "ninesql::ShowObjectDdl",
        "ninesql::CopyObjectDdl",
    ] {
        let spec = crate::actions::find(name).unwrap_or_else(|| panic!("{name}"));
        assert!(spec.in_palette, "{name}");
        assert!(!spec.default_keys.is_empty(), "{name}");
        assert_eq!(spec.category, "Navigator");
    }
}

fn tab_count(f: &Fixture) -> usize {
    f.ws.read_with(&f.vcx, |ws, cx| ws.tab_titles(cx).len())
}

fn active_text(f: &Fixture) -> Option<String> {
    let tab = f.ws.read_with(&f.vcx, |ws, _| ws.active_editor().cloned());
    tab.map(|t| t.read_with(&f.vcx, |t, cx| t.editor().read(cx).text(cx)))
}

/// Review M3: a filtered column list never yields a key condition, and says so.
#[gpui::test]
fn filtered_columns_give_no_key_condition(cx: &mut TestAppContext) {
    let mut f = connected(cx, MetadataConfig::default());
    let metadata = metadata_of(&f);
    let id = f.id;
    let filters = ninesql_core::ConnectionFilters::new()
        .with(
            ObjectKind::Column,
            ninesql_core::KindFilter::new(Vec::<String>::new(), vec!["id"], false)
                .unwrap_or_else(|e| panic!("{e}")),
        )
        .unwrap_or_else(|e| panic!("{e}"));
    let reply = metadata.set_filters(id, filters);
    let mut reply = Some(reply);
    wait_until(&mut f.vcx, |_| {
        matches!(
            reply.as_mut().map(|r| r.try_take()),
            Some(ninesql_core::Next::Ready(_))
        )
    });
    reply.take();
    f.ws.update_in(&mut f.vcx, |ws, window, cx| {
        ws.generate(id, path_of("t000"), Template::Delete, window, cx);
    });
    let ws = f.ws.clone();
    wait_until(&mut f.vcx, |vcx| {
        ws.read_with(vcx, |ws, cx| {
            ws.active_title(cx)
                .is_some_and(|t| t.as_ref() == "DELETE t000")
        })
    });
    assert_eq!(
        active_text(&f).as_deref(),
        Some(
            format!(
                "{}\nDELETE FROM \"public\".\"t000\"\nWHERE <condition>;\n",
                super::generate::KEY_UNKNOWN
            )
            .as_str()
        )
    );
    f.ws.update_in(&mut f.vcx, |ws, window, cx| {
        ws.generate(id, path_of("t000"), Template::Select, window, cx);
    });
    let ws = f.ws.clone();
    wait_until(&mut f.vcx, |vcx| {
        ws.read_with(vcx, |ws, cx| {
            ws.active_title(cx)
                .is_some_and(|t| t.as_ref() == "SELECT t000")
        })
    });
    let text = active_text(&f).unwrap_or_default();
    assert!(
        text.starts_with(super::generate::COLUMNS_INCOMPLETE),
        "{text}"
    );
    assert!(!text.contains("\"id\""), "{text}");
}

/// Review M2: a name the editor would change gets no template.
#[gpui::test]
fn names_the_editor_would_change_get_no_template(cx: &mut TestAppContext) {
    let mut f = connected(cx, MetadataConfig::default());
    let id = f.id;
    let before = tab_count(&f);
    for name in ["orders\u{1}", "orders\r", "a\u{2028}b"] {
        f.ws.update_in(&mut f.vcx, |ws, window, cx| {
            ws.generate(id, path_of(name), Template::Select, window, cx);
        });
        f.vcx.run_until_parked();
        assert_eq!(tab_count(&f), before, "{name:?}");
    }
}

/// Review L4: no write template on a read-only session.
#[gpui::test]
fn read_only_sessions_offer_select_only(cx: &mut TestAppContext) {
    let mut f = connected_with(cx, MetadataConfig::default(), true);
    let id = f.id;
    let read_only = f.hub.read_with(&f.vcx, |h, _| {
        h.sessions(id).first().is_some_and(|s| s.info().read_only)
    });
    assert!(read_only, "the mock opened a read-only session");
    let before = tab_count(&f);
    for which in [Template::Insert, Template::Update, Template::Delete] {
        f.ws.update_in(&mut f.vcx, |ws, window, cx| {
            ws.generate(id, path_of("t000"), which, window, cx);
        });
        f.vcx.run_until_parked();
        assert_eq!(tab_count(&f), before, "{which:?}");
    }
    f.ws.update_in(&mut f.vcx, |ws, window, cx| {
        ws.generate(id, path_of("t000"), Template::Select, window, cx);
    });
    let ws = f.ws.clone();
    wait_until(&mut f.vcx, |vcx| {
        ws.read_with(vcx, |ws, cx| {
            ws.active_title(cx)
                .is_some_and(|t| t.as_ref() == "SELECT t000")
        })
    });
    f.ws.update_in(&mut f.vcx, |ws, window, cx| {
        ws.open_object_editor(id, path_of("t000"), None, window, cx);
    });
    let editor =
        f.ws.read_with(&f.vcx, |ws, _| ws.active_object_editor().cloned());
    assert!(editor.is_some_and(|e| e.read_with(&f.vcx, |e, cx| e.read_only(cx))));
}

#[test]
fn the_context_menu_of_tables_and_views_starts_with_open_table_data() {
    use crate::navigator::object_menu_entries;
    let allowance = |read_only: bool| {
        if read_only {
            TemplateAllowance::select_only()
        } else {
            TemplateAllowance::all()
        }
    };
    let labels = |kind: Option<ObjectKind>, read_only: bool, ddl: bool| -> Vec<String> {
        object_menu_entries(kind, allowance(read_only), ddl)
            .into_iter()
            .map(|e| e.map_or_else(|| "-".to_owned(), |(label, _)| label))
            .collect()
    };
    for kind in [
        ObjectKind::Table,
        ObjectKind::View,
        ObjectKind::MaterializedView,
    ] {
        let entries = labels(Some(kind), false, true);
        assert_eq!(entries.first().map(String::as_str), Some("Open table data"));
        assert_eq!(
            entries.get(1).map(String::as_str),
            Some("Open object editor")
        );
        // Reading the rows is offered on a read-only session too (the action only reads).
        let read_only = labels(Some(kind), true, false);
        assert_eq!(
            read_only.first().map(String::as_str),
            Some("Open table data")
        );
        assert!(
            !read_only.iter().any(|l| l == "Generate SQL: DELETE"),
            "{read_only:?}"
        );
        // Review L2: row writes only on tables.
        assert_eq!(
            entries.iter().any(|l| l == "Generate SQL: DELETE"),
            kind == ObjectKind::Table,
            "{kind:?}: {entries:?}"
        );
    }
    let entries = object_menu_entries(Some(ObjectKind::Table), TemplateAllowance::all(), false);
    let first = entries
        .first()
        .and_then(|e| e.as_ref())
        .map(|(_, a)| a.name());
    assert_eq!(first, Some("ninesql::OpenTableData"));
    // Not offered where the action refuses (a column, a schema).
    for kind in [ObjectKind::Column, ObjectKind::Schema] {
        assert!(!labels(Some(kind), false, true).contains(&"Open table data".to_owned()));
    }
    assert_eq!(labels(Some(ObjectKind::Schema), false, false), ["-"]);
}

#[gpui::test]
fn open_table_data_from_the_context_menu_opens_the_table(cx: &mut TestAppContext) {
    let mut f = connected(cx, MetadataConfig::default());
    open_tables(&mut f);
    let ix = index_of(&f, "t000");
    f.nav
        .update(&mut f.vcx, |n, cx| n.select_index(Some(ix), cx));
    let entries = crate::navigator::object_menu_entries(
        Some(ObjectKind::Table),
        TemplateAllowance::all(),
        false,
    );
    let Some(Some((_, action))) = entries.into_iter().next() else {
        panic!("a first entry");
    };
    f.ws.update_in(&mut f.vcx, |ws, window, cx| {
        ws.perform(action.as_ref(), window, cx);
    });
    let tab = f.ws.read_with(&f.vcx, |ws, _| ws.active_editor().cloned());
    let Some(tab) = tab else {
        panic!("a table tab");
    };
    assert!(tab.read_with(&f.vcx, |t, _| t.table_state().source.is_some()));
    wait_until(&mut f.vcx, |vcx| {
        tab.read_with(vcx, |t, cx| {
            t.table_state().describing.is_none()
                && t.table_state().source_request.is_some()
                && !t.is_running()
                && t.editor().read(cx).running().is_none()
        })
    });
}

/// Review L2: "Generate SQL" follows the driver's declared `Capabilities::editing`. A driver
/// that declares no row editing (QuestDB) gets the SELECT template only: in the context menu, in
/// the object editor's toolbar and through the actions (palette, key bindings).
#[gpui::test]
fn generate_sql_follows_the_declared_editing_capabilities(cx: &mut TestAppContext) {
    use ninesql_core::driver_api::capabilities::EditingCapabilities;
    use ninesql_core::driver_api::mock::{MockDriver, full_capabilities};
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let mut caps = full_capabilities();
    caps.editing = EditingCapabilities::default();
    let c = mock_connection("NoEdit", ConnectionTypeId::dev(), Auth::None);
    let id = c.id;
    let hub = crate::connections::tests::mock_hub_with_transfer_driver(
        cx,
        tmp.path(),
        vec![c],
        MockDriver::new().with_capabilities(caps),
    );
    let (ws, mut vcx) = open_live(cx);
    ws.update_in(&mut vcx, |ws, window, cx| {
        ws.connect(id, Purpose::Open, window, cx);
    });
    wait_until(&mut vcx, |vcx| hub.read_with(vcx, |h, _| h.is_open(id)));
    let nav = ws.read_with(&vcx, |ws, _| ws.navigator().clone());
    let mut f = Fixture {
        ws,
        hub,
        nav,
        id,
        vcx,
        _tmp: tmp,
    };
    expand(&mut f, "NoEdit");
    wait_for(&mut f, "nine");
    expand(&mut f, "nine");
    wait_for(&mut f, "public");
    expand(&mut f, "public");
    wait_for(&mut f, "t000");
    let ix = index_of(&f, "t000");
    f.nav
        .update(&mut f.vcx, |n, cx| n.select_index(Some(ix), cx));
    let labels: Vec<String> = f
        .nav
        .read_with(&f.vcx, |n, cx| n.selected_object_menu(cx))
        .into_iter()
        .flatten()
        .map(|(l, _)| l)
        .collect();
    assert!(
        labels.contains(&"Generate SQL: SELECT".to_owned()),
        "{labels:?}"
    );
    for write in ["INSERT", "UPDATE", "DELETE"] {
        assert!(
            !labels.contains(&format!("Generate SQL: {write}")),
            "{write}: {labels:?}"
        );
    }
    // The actions refuse too.
    let before = tab_count(&f);
    for action in [
        Box::new(crate::actions::GenerateInsertSql) as Box<dyn gpui::Action>,
        Box::new(crate::actions::GenerateUpdateSql),
        Box::new(crate::actions::GenerateDeleteSql),
    ] {
        f.ws.update_in(&mut f.vcx, |ws, window, cx| {
            ws.perform(action.as_ref(), window, cx);
        });
        f.vcx.run_until_parked();
        assert_eq!(tab_count(&f), before, "{}", action.name());
    }
    // The object editor's toolbar offers SELECT only.
    f.ws.update_in(&mut f.vcx, |ws, window, cx| {
        ws.open_object_editor(id, path_of("t000"), None, window, cx);
    });
    let editor =
        f.ws.read_with(&f.vcx, |ws, _| ws.active_object_editor().cloned());
    let Some(editor) = editor else {
        panic!("an object editor");
    };
    assert_eq!(
        editor.read_with(&f.vcx, |e, cx| e.offered_templates(cx)),
        vec![Template::Select]
    );
}

/// Review L2: on a driver that declares row editing, the object editor of a table offers every
/// template, and a view gets no write template.
#[gpui::test]
fn write_templates_are_offered_on_tables_only(cx: &mut TestAppContext) {
    let mut f = connected(cx, MetadataConfig::default());
    let id = f.id;
    f.ws.update_in(&mut f.vcx, |ws, window, cx| {
        ws.open_object_editor(id, path_of("t000"), None, window, cx);
    });
    let editor =
        f.ws.read_with(&f.vcx, |ws, _| ws.active_object_editor().cloned());
    let Some(editor) = editor else {
        panic!("an object editor");
    };
    assert_eq!(
        editor.read_with(&f.vcx, |e, cx| e.offered_templates(cx)),
        Template::ALL.to_vec()
    );
    let view = ObjectPath::root()
        .child(ObjectKind::Catalog, "nine")
        .child(ObjectKind::Schema, "public")
        .child(ObjectKind::View, "v000");
    let before = tab_count(&f);
    f.ws.update_in(&mut f.vcx, |ws, window, cx| {
        ws.generate(id, view, Template::Delete, window, cx);
    });
    f.vcx.run_until_parked();
    assert_eq!(tab_count(&f), before, "no DELETE template on a view");
}

/// Review L8: the navigator's hints name the bindings in effect, not hard-coded ones.
#[test]
fn navigator_hints_follow_the_effective_key_bindings() {
    use crate::keymap::{BindingOverrides, EffectiveKeymap};
    use crate::navigator::{no_connections_text, not_connected_text};
    let defaults = EffectiveKeymap::resolve(&BindingOverrides::default());
    assert_eq!(
        not_connected_text(Some(&defaults)),
        "Not connected (\"Connect\": Ctrl+K Ctrl+O)"
    );
    assert_eq!(
        no_connections_text(Some(&defaults)),
        "No connections yet. Use \"New connection\" (Ctrl+Alt+N) or \"Import from DBeaver\" (Ctrl+K Ctrl+I)."
    );
    let mut overrides = BindingOverrides::default();
    overrides.set("ninesql::ConnectConnection", vec!["alt-shift-c".into()]);
    overrides.set("ninesql::NewConnection", vec![]);
    let changed = EffectiveKeymap::resolve(&overrides);
    assert_eq!(
        not_connected_text(Some(&changed)),
        "Not connected (\"Connect\": Alt+Shift+C)"
    );
    assert_eq!(
        no_connections_text(Some(&changed)),
        "No connections yet. Use \"New connection\" or \"Import from DBeaver\" (Ctrl+K Ctrl+I)."
    );
    overrides.set("ninesql::ConnectConnection", vec![]);
    let unbound = EffectiveKeymap::resolve(&overrides);
    assert_eq!(
        not_connected_text(Some(&unbound)),
        "Not connected (\"Connect\" in the context menu)"
    );
    assert_eq!(
        not_connected_text(None),
        "Not connected (\"Connect\" in the context menu)"
    );
}

/// Review L8: an override from `settings.toml` reaches the hint through the installed keymap,
/// which is what the navigator reads when it draws a row.
#[gpui::test]
fn the_installed_keymap_drives_the_navigator_hint(cx: &mut TestAppContext) {
    use crate::keymap::EffectiveKeymap;
    use crate::navigator::not_connected_text;
    let (settings, diagnostics) =
        crate::settings::parse("[keybindings]\nConnectConnection = \"alt-shift-c\"\n");
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    cx.update(|cx| {
        crate::app::init(
            crate::settings::SettingsState {
                path: None,
                settings,
                diagnostics,
                save_blocked: None,
            },
            cx,
        );
    });
    let text = cx.update(|cx| not_connected_text(cx.try_global::<EffectiveKeymap>()));
    assert_eq!(text, "Not connected (\"Connect\": Alt+Shift+C)");
}
