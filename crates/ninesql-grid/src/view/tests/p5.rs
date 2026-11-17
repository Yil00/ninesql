//! `TestAppContext` tests of the P5-A parts of the results view: view modes, panels,
//! client-side sort / filter through the key bindings, advanced copy.

use std::sync::Arc;

use gpui::{TestAppContext, VisualTestContext};
use ninesql_core::driver_api::{AnsiDialect, ResultItem};

use super::{batch, clipboard, header, item, open, select_cell, send, small_budget};
use crate::rows::{ClientFilter, FilterOp};
use crate::selection::GridSelection;
use crate::source::ExecutionEvent;
use crate::view::{ResultsView, SidePanel, ViewMode};
use crate::viewer::ViewerKind;

fn loaded(cx: &mut TestAppContext, rows: usize) -> (gpui::Entity<ResultsView>, VisualTestContext) {
    let (view, _, mut vcx) = open(cx, small_budget());
    send(
        &view,
        &mut vcx,
        vec![
            ExecutionEvent::Started,
            item(0, header()),
            item(0, ResultItem::Rows(batch(0, rows))),
        ],
    );
    (view, vcx)
}

#[gpui::test]
fn sort_and_filter_loaded_rows_through_key_bindings(cx: &mut TestAppContext) {
    let (view, mut vcx) = loaded(cx, 10);
    select_cell(&view, &mut vcx, 0, 0);
    vcx.simulate_keystrokes("alt-s d");
    let label = view.read_with(&vcx, |v, cx| v.rows_label(cx)).unwrap();
    assert!(label.starts_with("Loaded rows only"), "{label}");
    assert!(label.contains("sorted by id ↓"), "{label}");
    // Display row 0 is now id 9: copy follows the view.
    select_cell(&view, &mut vcx, 0, 0);
    vcx.simulate_keystrokes("ctrl-c");
    assert_eq!(clipboard(&vcx), "9");
    // Filter by the selected value, then clear.
    vcx.simulate_keystrokes("alt-s v");
    assert_eq!(view.read_with(&vcx, |v, cx| v.shown_rows(cx)), Some(1));
    vcx.simulate_keystrokes("alt-s c");
    assert_eq!(view.read_with(&vcx, |v, cx| v.shown_rows(cx)), Some(10));
    assert!(view.read_with(&vcx, |v, cx| v.rows_label(cx)).is_none());
}

#[gpui::test]
fn filter_bar_applies_on_enter(cx: &mut TestAppContext) {
    let (view, mut vcx) = loaded(cx, 10);
    select_cell(&view, &mut vcx, 0, 0);
    vcx.simulate_keystrokes("alt-s f");
    assert!(view.read_with(&vcx, |v, _| v.filter_bar_open()));
    vcx.simulate_input(">= 7");
    vcx.simulate_keystrokes("enter");
    assert!(!view.read_with(&vcx, |v, _| v.filter_bar_open()));
    assert_eq!(view.read_with(&vcx, |v, cx| v.shown_rows(cx)), Some(3));
    let label = view.read_with(&vcx, |v, cx| v.rows_label(cx)).unwrap();
    assert!(label.contains("id ≥ \"7\""), "{label}");
    assert!(label.contains("3 of 10 loaded rows shown"), "{label}");
}

#[gpui::test]
fn view_modes_and_record_navigation(cx: &mut TestAppContext) {
    let (view, mut vcx) = loaded(cx, 5);
    select_cell(&view, &mut vcx, 1, 0);
    vcx.simulate_keystrokes("alt-2");
    assert_eq!(view.read_with(&vcx, |v, _| v.mode()), ViewMode::Record);
    let fields = view.read_with(&vcx, |v, cx| v.record_fields(cx));
    assert_eq!(fields[0].text, "1");
    assert_eq!(fields[1].text, "n1");
    vcx.simulate_keystrokes("alt-down alt-down");
    let fields = view.read_with(&vcx, |v, cx| v.record_fields(cx));
    assert_eq!(fields[0].text, "3");
    assert!(fields[1].is_null);
    vcx.simulate_keystrokes("alt-up");
    assert_eq!(
        view.read_with(&vcx, |v, cx| v.record_fields(cx))[0].text,
        "2"
    );
    vcx.simulate_keystrokes("alt-3");
    assert_eq!(view.read_with(&vcx, |v, _| v.mode()), ViewMode::Text);
    let text = view.update(&mut vcx, |v, cx| v.text_view(cx)).unwrap();
    assert!(text.text.starts_with("id | name\n"), "{}", text.text);
    assert_eq!(text.rows, 5);
    vcx.simulate_keystrokes("alt-1");
    assert_eq!(view.read_with(&vcx, |v, _| v.mode()), ViewMode::Grid);
}

#[gpui::test]
fn panels_toggle_and_show_the_selection(cx: &mut TestAppContext) {
    let (view, mut vcx) = loaded(cx, 10);
    vcx.simulate_keystrokes("alt-v");
    assert_eq!(
        view.read_with(&vcx, |v, _| v.panel()),
        Some(SidePanel::Value)
    );
    select_cell(&view, &mut vcx, 4, 1);
    let i = view.update(&mut vcx, |v, cx| v.inspection(cx)).unwrap();
    assert_eq!(i.text, "n4");
    assert_eq!(i.viewers, vec![ViewerKind::Text]);
    vcx.simulate_keystrokes("alt-a");
    assert_eq!(
        view.read_with(&vcx, |v, _| v.panel()),
        Some(SidePanel::Aggregate)
    );
    vcx.simulate_keystrokes("ctrl-a");
    let a = view.update(&mut vcx, |v, cx| v.aggregates(cx)).unwrap();
    assert_eq!(a.rows, 10);
    assert_eq!(a.columns[0].sum_text.as_deref(), Some("45"));
    vcx.simulate_keystrokes("alt-m");
    assert_eq!(
        view.read_with(&vcx, |v, _| v.panel()),
        Some(SidePanel::Metadata)
    );
    let info = view.read_with(&vcx, |v, cx| v.column_info(cx));
    assert_eq!(info.len(), 2);
    assert_eq!(info[0].source, None, "unknown unless the owner says");
    vcx.simulate_keystrokes("alt-m");
    assert_eq!(view.read_with(&vcx, |v, _| v.panel()), None);
}

#[gpui::test]
fn advanced_copy_formats(cx: &mut TestAppContext) {
    let (view, mut vcx) = loaded(cx, 3);
    select_cell(&view, &mut vcx, 0, 0);
    vcx.simulate_keystrokes("shift-down shift-right");
    vcx.simulate_keystrokes("ctrl-alt-m");
    assert_eq!(
        clipboard(&vcx),
        "| id | name |\n| --- | --- |\n| 0 | n0 |\n| 1 | n1 |\n"
    );
    vcx.simulate_keystrokes("ctrl-alt-j");
    assert!(
        clipboard(&vcx).contains("\"name\": \"n1\""),
        "{}",
        clipboard(&vcx)
    );
    // SQL INSERT without a dialect: refused, said in the status line.
    vcx.simulate_keystrokes("ctrl-alt-i");
    let status = view
        .read_with(&vcx, |v, _| v.status().map(str::to_owned))
        .unwrap();
    assert!(status.contains("dialect"), "{status}");
    view.update(&mut vcx, |v, _| {
        v.set_dialect(Some(Arc::new(AnsiDialect)));
        v.set_source_table(Some(vec!["s".into(), "t".into()]));
    });
    vcx.simulate_keystrokes("ctrl-alt-i");
    assert_eq!(
        clipboard(&vcx),
        "INSERT INTO \"s\".\"t\" (\"id\", \"name\") VALUES (0, 'n0');\nINSERT INTO \"s\".\"t\" (\"id\", \"name\") VALUES (1, 'n1');\n"
    );
}

#[gpui::test]
fn client_filter_survives_new_pages(cx: &mut TestAppContext) {
    let (view, mut vcx) = loaded(cx, 10);
    view.update(&mut vcx, |v, cx| {
        v.add_client_filter(ClientFilter::on(0, FilterOp::Less, "3"), cx);
    });
    assert_eq!(view.read_with(&vcx, |v, cx| v.shown_rows(cx)), Some(3));
    view.update(&mut vcx, |v, cx| {
        v.select(GridSelection::All, cx);
    });
    send(
        &view,
        &mut vcx,
        vec![
            item(0, ResultItem::Rows(batch(10, 5))),
            ExecutionEvent::StatementFinished { statement: 0 },
        ],
    );
    // New rows are filtered too once the page is in.
    assert_eq!(view.read_with(&vcx, |v, cx| v.shown_rows(cx)), Some(3));
}

fn png_value(width: u32, height: u32) -> Vec<u8> {
    let mut v = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR".to_vec();
    v.extend_from_slice(&width.to_be_bytes());
    v.extend_from_slice(&height.to_be_bytes());
    v.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
    v.extend_from_slice(b"\x00\x00\x00\x00IEND\x00\x00\x00\x00");
    v
}

#[gpui::test]
fn preview_images_are_evicted_and_outputs_cached(cx: &mut TestAppContext) {
    use ninesql_core::driver_api::ResultSetHeader;
    use ninesql_core::driver_api::arrow_array::{BinaryArray, RecordBatch};
    use ninesql_core::driver_api::arrow_schema::{DataType, Field, Schema};

    let (view, _, mut vcx) = open(cx, small_budget());
    let schema = Arc::new(Schema::new(vec![Field::new("b", DataType::Binary, true)]));
    let a = png_value(2, 2);
    let b = png_value(3, 3);
    let rows = RecordBatch::try_new(
        schema.clone(),
        vec![Arc::new(BinaryArray::from(vec![
            Some(a.as_slice()),
            Some(b.as_slice()),
        ]))],
    )
    .unwrap();
    send(
        &view,
        &mut vcx,
        vec![
            ExecutionEvent::Started,
            item(
                0,
                ResultItem::ResultSet(ResultSetHeader::tabular(0, schema)),
            ),
            item(0, ResultItem::Rows(rows)),
        ],
    );
    vcx.simulate_keystrokes("alt-v");
    select_cell(&view, &mut vcx, 0, 0);
    vcx.run_until_parked();
    let first = view
        .read_with(&vcx, |v, _| v.held_images())
        .0
        .expect("first image")
        .image();
    assert!(vcx.update(|_, cx| first.is_asset_cached(cx)));
    // The viewer output is cached: the same Rc until the focus moves.
    let o1 = view.update(&mut vcx, |v, cx| {
        let i = v.inspection(cx).unwrap();
        v.viewer_output(&i, ViewerKind::Image, cx)
    });
    let o2 = view.update(&mut vcx, |v, cx| {
        let i = v.inspection(cx).unwrap();
        v.viewer_output(&i, ViewerKind::Image, cx)
    });
    assert!(std::rc::Rc::ptr_eq(&o1, &o2));
    // Another cell: the first image leaves GPUI's asset cache.
    select_cell(&view, &mut vcx, 1, 0);
    vcx.run_until_parked();
    let (second, stale) = view.read_with(&vcx, |v, _| v.held_images());
    let second = second.expect("second image").image();
    assert_eq!(stale, 0);
    assert!(!vcx.update(|_, cx| first.is_asset_cached(cx)));
    assert!(vcx.update(|_, cx| second.is_asset_cached(cx)));
    // Closing the panel evicts the shown image too.
    vcx.simulate_keystrokes("alt-v");
    vcx.run_until_parked();
    let (held, stale) = view.read_with(&vcx, |v, _| v.held_images());
    assert!(held.is_none());
    assert_eq!(stale, 0);
    assert!(!vcx.update(|_, cx| second.is_asset_cached(cx)));
}

/// A valid PNG (`side` × `side`, 8-bit grey, stored deflate blocks), so GPUI decodes it and
/// paints it into the sprite atlas.
fn valid_png(side: u32) -> Vec<u8> {
    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = !0u32;
        for b in bytes {
            crc ^= u32::from(*b);
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }
    fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(&u32::try_from(data.len()).unwrap().to_be_bytes());
        let mut body = kind.to_vec();
        body.extend_from_slice(data);
        out.extend_from_slice(&body);
        out.extend_from_slice(&crc32(&body).to_be_bytes());
    }
    let mut raw = Vec::new();
    for _ in 0..side {
        raw.push(0); // filter: none
        raw.extend(std::iter::repeat_n(0x80u8, side as usize));
    }
    let (mut a, mut b) = (1u32, 0u32);
    for x in &raw {
        a = (a + u32::from(*x)) % 65_521;
        b = (b + a) % 65_521;
    }
    let len = u16::try_from(raw.len()).unwrap();
    let mut zlib = vec![0x78, 0x01, 0x01];
    zlib.extend_from_slice(&len.to_le_bytes());
    zlib.extend_from_slice(&(!len).to_le_bytes());
    zlib.extend_from_slice(&raw);
    zlib.extend_from_slice(&((b << 16) | a).to_be_bytes());
    let mut ihdr = side.to_be_bytes().to_vec();
    ihdr.extend_from_slice(&side.to_be_bytes());
    ihdr.extend_from_slice(&[8, 0, 0, 0, 0]);
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &zlib);
    chunk(&mut out, b"IEND", &[]);
    out
}

/// Holds a results view that a test can drop while the window stays open.
struct Holder(Option<gpui::Entity<ResultsView>>);

impl gpui::Render for Holder {
    fn render(
        &mut self,
        _: &mut gpui::Window,
        _: &mut gpui::Context<Self>,
    ) -> impl gpui::IntoElement {
        use gpui::{ParentElement as _, Styled as _};
        gpui::div().size_full().children(self.0.clone())
    }
}

/// A view closed while its preview image is shown takes the image out of the sprite atlas of
/// every window, not only out of GPUI's asset cache (phase 5 re-review M-B, I6).
#[gpui::test]
fn closing_the_view_drops_a_shown_image_from_the_atlas(cx: &mut TestAppContext) {
    use ninesql_core::driver_api::ResultSetHeader;
    use ninesql_core::driver_api::arrow_array::{BinaryArray, RecordBatch};
    use ninesql_core::driver_api::arrow_schema::{DataType, Field, Schema};

    use gpui::AppContext as _;

    super::init(cx);
    let window = cx.add_window(|_, cx| {
        let view = cx.new(|cx| ResultsView::new(small_budget(), cx));
        Holder(Some(view))
    });
    let holder = window.root(cx).unwrap();
    let mut vcx = VisualTestContext::from_window(window.into(), cx);
    let view = holder.read_with(&vcx, |h, _| h.0.clone()).unwrap();
    let schema = Arc::new(Schema::new(vec![Field::new("b", DataType::Binary, true)]));
    let png = valid_png(4);
    let rows = RecordBatch::try_new(
        schema.clone(),
        vec![Arc::new(BinaryArray::from(vec![Some(png.as_slice())]))],
    )
    .unwrap();
    send(
        &view,
        &mut vcx,
        vec![
            ExecutionEvent::Started,
            item(
                0,
                ResultItem::ResultSet(ResultSetHeader::tabular(0, schema)),
            ),
            item(0, ResultItem::Rows(rows)),
        ],
    );
    vcx.simulate_keystrokes("alt-v");
    select_cell(&view, &mut vcx, 0, 0);
    vcx.run_until_parked();
    // Another frame once the decode is in: the image is painted.
    vcx.update(|window, _| window.refresh());
    vcx.run_until_parked();
    let cached = view
        .read_with(&vcx, |v, _| v.held_images())
        .0
        .expect("shown image");
    let image = cached.image();
    let render = cached
        .render()
        .expect("the decoded image is kept once shown");
    drop(cached);
    assert!(vcx.update(|window, _| window.has_image_atlas_entry(&render)));
    assert!(vcx.update(|_, cx| image.is_asset_cached(cx)));
    // Close the view while the image is shown.
    drop(view);
    vcx.update(|_, cx| {
        holder.update(cx, |h, cx| {
            h.0 = None;
            cx.notify();
        });
    });
    vcx.run_until_parked();
    assert!(!vcx.update(|_, cx| image.is_asset_cached(cx)));
    assert!(!vcx.update(|window, _| window.has_image_atlas_entry(&render)));
}

/// An image no frame laid out is evicted without a decode being started for it, while one
/// that was laid out is looked up in GPUI's cache (phase 5 re-review L-B).
#[gpui::test]
fn an_image_never_shown_starts_no_decode_when_evicted(cx: &mut TestAppContext) {
    use crate::view::panels::CachedImage;

    let (_view, _, mut vcx) = open(cx, small_budget());
    let image = Arc::new(gpui::Image::from_bytes(
        gpui::ImageFormat::Png,
        valid_png(2),
    ));
    let never_shown = CachedImage::new(image.clone());
    assert!(
        vcx.update(|window, cx| never_shown.take_decoded(Some(window), cx))
            .is_none()
    );
    assert!(!vcx.update(|_, cx| image.is_asset_cached(cx)));
    // The same check sees a lookup: a shown image asks GPUI's cache.
    let shown = CachedImage::new(image.clone());
    shown.mark_shown();
    let _ = vcx.update(|window, cx| shown.take_decoded(Some(window), cx));
    assert!(vcx.update(|_, cx| image.is_asset_cached(cx)));
    vcx.update(|_, cx| image.clone().remove_asset(cx));
}
