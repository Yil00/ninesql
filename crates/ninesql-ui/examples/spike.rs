//! P0-A GPUI spike (ROADMAP P0-A, ADR-0001). Throwaway: not part of the shipped binary.
//!
//! One window with a dock (navigator on the left, output at the bottom), a center tab group
//! holding two SQL editors (tree-sitter SQL highlighting from `gpui-component`) and a virtual
//! table over 1 000 000 synthetic rows. Cells are computed from `(row, column)` only when the
//! table asks for a visible cell, so no row is ever stored (I6).
//!
//! Run: `cargo run -p ninesql-ui --example spike --release`
//!
//! Environment (all optional):
//! - `NINESQL_SPIKE_METRICS=1`: print `spike-metric key=value` lines on stderr (first frame,
//!   RSS, frame times). Never prints a cell value.
//! - `NINESQL_SPIKE_AUTO=1`: scripted benchmark (implies metrics): idle RSS with an empty
//!   table, load the rows, programmatic scroll, random jumps, clipboard round trip, then quit
//!   (unless `NINESQL_SPIKE_STAY=1`).
//! - `NINESQL_SPIKE_ROWS=<n>`: row count (default 1 000 000).
//! - `NINESQL_SPIKE_SCROLL_FRAMES=<n>`: frames of continuous scrolling in auto mode (600).
//! - `NINESQL_SPIKE_THEME=dark`: start in the dark theme.
//!
//! Keys: `ctrl-q` quit, `ctrl-shift-t` toggle light / dark, `ctrl-shift-b` run the scroll
//! benchmark, `ctrl-shift-y` clipboard round trip (result in the output panel).
#![forbid(unsafe_code)]

use std::cell::RefCell;
use std::io::Write as _;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui_kit::assets::Assets;
use gpui_kit::component::dock::{
    BasePanel, DockArea, DockLayout, DockPlacement, DockSkin, Panel, PanelEvent, panel_handle,
};
use gpui_kit::component::input::{Editor, EditorState};
use gpui_kit::component::table::{Column, DataTable, TableDelegate, TableState};
use gpui_kit::component::{ActiveTheme as _, Theme, ThemeMode, v_flex};
use gpui_kit::*;

actions!(
    spike,
    [Quit, ToggleTheme, RunScrollBench, ClipboardRoundTrip]
);

const DEFAULT_ROWS: usize = 1_000_000;
const DEFAULT_SCROLL_FRAMES: usize = 600;
/// Pixels scrolled per frame in the benchmark (about 2 rows of the default table size).
const SCROLL_STEP_PX: f32 = 48.0;
const JUMP_FRAMES: usize = 120;

const SAMPLE_SQL: &str = "-- NineSQL GPUI spike: tree-sitter SQL highlighting
WITH recent AS (
    SELECT o.id, o.customer_id, o.amount, o.created_at
    FROM orders AS o
    WHERE o.created_at >= now() - INTERVAL '30 days'
      AND o.status IN ('active', 'pending')
)
SELECT c.name, count(*) AS orders, sum(r.amount) AS total
FROM recent r
JOIN customers c ON c.id = r.customer_id
GROUP BY c.name
HAVING sum(r.amount) > 1000.50
ORDER BY total DESC
LIMIT 100;

UPDATE customers SET email = lower(email) WHERE email <> lower(email);
";

// ---------------------------------------------------------------------------------------------
// Metrics (env-gated, stderr only, never a cell value)
// ---------------------------------------------------------------------------------------------

#[derive(Clone)]
struct Metrics {
    enabled: bool,
    start: Instant,
}

impl Metrics {
    fn from_env(start: Instant) -> Self {
        let enabled = env_flag("NINESQL_SPIKE_METRICS") || env_flag("NINESQL_SPIKE_AUTO");
        Self { enabled, start }
    }

    fn since_start_ms(&self) -> String {
        format!("{:.1}", self.start.elapsed().as_secs_f64() * 1000.0)
    }

    fn emit(&self, key: &str, value: impl std::fmt::Display) {
        if self.enabled {
            // Direct stderr write: this is an example binary, not a library crate.
            let _ = writeln!(std::io::stderr(), "spike-metric {key}={value}");
        }
    }

    fn emit_rss(&self, key: &str) {
        match rss_kib() {
            Some(kib) => self.emit(key, format!("{:.1}MiB", kib as f64 / 1024.0)),
            None => self.emit(key, "n/a"),
        }
    }
}

fn env_flag(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.replace('_', "").parse().ok())
        .unwrap_or(default)
}

/// Resident set size of this process in KiB (Linux `/proc`); `None` elsewhere.
fn rss_kib() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
}

/// Frame interval statistics, in milliseconds.
#[derive(Debug)]
struct FrameStats {
    count: usize,
    mean: f64,
    p50: f64,
    p95: f64,
    p99: f64,
    max: f64,
}

impl FrameStats {
    fn from_intervals(intervals: &[f64]) -> Option<Self> {
        let mut sorted = intervals.to_vec();
        sorted.sort_by(f64::total_cmp);
        let last = sorted.len().checked_sub(1)?;
        let pick = |q: f64| sorted[((last as f64 * q).round() as usize).min(last)];
        Some(Self {
            count: sorted.len(),
            mean: sorted.iter().sum::<f64>() / sorted.len() as f64,
            p50: pick(0.50),
            p95: pick(0.95),
            p99: pick(0.99),
            max: sorted[last],
        })
    }

    fn summary(&self) -> String {
        format!(
            "frames={} mean={:.2}ms p50={:.2}ms p95={:.2}ms p99={:.2}ms max={:.2}ms fps_mean={:.1}",
            self.count,
            self.mean,
            self.p50,
            self.p95,
            self.p99,
            self.max,
            1000.0 / self.mean
        )
    }
}

// ---------------------------------------------------------------------------------------------
// Synthetic rows: computed per visible cell, never materialized (I6)
// ---------------------------------------------------------------------------------------------

const COLUMNS: [(&str, f32); 8] = [
    ("id", 90.0),
    ("uuid", 170.0),
    ("name", 150.0),
    ("email", 260.0),
    ("amount", 110.0),
    ("created_at", 170.0),
    ("status", 100.0),
    ("note", 220.0),
];
const FIRST: [&str; 8] = [
    "Ada", "Brian", "Chloe", "Dmitri", "Elena", "Farid", "Grace", "Hiro",
];
const LAST: [&str; 8] = [
    "Lovelace",
    "Kernighan",
    "Martin",
    "Ivanov",
    "Rossi",
    "Haddad",
    "Hopper",
    "Tanaka",
];
const STATUS: [&str; 4] = ["active", "pending", "disabled", "archived"];
const NOTES: [&str; 4] = [
    "lorem ipsum dolor sit amet",
    "consectetur adipiscing",
    "sed do eiusmod tempor",
    "ut labore et dolore magna",
];

/// splitmix64 over `(a, b)`: deterministic and allocation free.
fn mix(a: u64, b: u64) -> u64 {
    let mut z = a
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(b.wrapping_mul(0xBF58_476D_1CE4_E5B9));
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// The text of one synthetic cell. Only called for cells on screen.
fn synthetic_cell(row: usize, col: usize) -> String {
    let h = mix(row as u64, col as u64);
    let person = mix(row as u64, 0);
    let first = FIRST[(person % 8) as usize];
    let last = LAST[((person >> 8) % 8) as usize];
    match col {
        0 => (row + 1).to_string(),
        1 => format!(
            "{:08x}-{:04x}-{:04x}",
            h >> 32,
            (h >> 16) & 0xFFFF,
            h & 0xFFFF
        ),
        2 => format!("{first} {last}"),
        3 => format!(
            "{}.{}{row}@example.test",
            first.to_ascii_lowercase(),
            last.to_ascii_lowercase()
        ),
        4 => format!("{:.2}", (h % 10_000_000) as f64 / 100.0),
        5 => format!(
            "20{:02}-{:02}-{:02} {:02}:{:02}:{:02}",
            20 + h % 6,
            1 + (h >> 8) % 12,
            1 + (h >> 16) % 28,
            (h >> 24) % 24,
            (h >> 32) % 60,
            (h >> 40) % 60
        ),
        6 => STATUS[(h % 4) as usize].to_string(),
        7 if row % 7 == 3 => "NULL".to_string(),
        7 => NOTES[(h % 4) as usize].to_string(),
        _ => String::new(),
    }
}

struct SyntheticRows {
    rows: usize,
}

impl TableDelegate for SyntheticRows {
    fn columns_count(&self, _cx: &App) -> usize {
        COLUMNS.len()
    }

    fn rows_count(&self, _cx: &App) -> usize {
        self.rows
    }

    fn column(&self, col_ix: usize, _cx: &App) -> Column {
        let (name, width) = COLUMNS[col_ix.min(COLUMNS.len() - 1)];
        let column = Column::new(name, name).width(px(width));
        if col_ix == 0 || col_ix == 4 {
            column.text_right()
        } else {
            column
        }
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _window: &mut Window,
        _cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        synthetic_cell(row_ix, col_ix)
    }

    fn cell_label(&self, row_ix: usize, col_ix: usize, _cx: &App) -> Option<SharedString> {
        Some(synthetic_cell(row_ix, col_ix).into())
    }
}

// ---------------------------------------------------------------------------------------------
// Panels
// ---------------------------------------------------------------------------------------------

struct SqlEditorPanel {
    title: SharedString,
    editor: Entity<EditorState>,
}

impl SqlEditorPanel {
    fn new(title: &str, sql: &str, window: &mut Window, cx: &mut App) -> Entity<Self> {
        let sql = sql.to_string();
        let title = SharedString::from(title.to_string());
        cx.new(|cx| {
            let editor = cx.new(|cx| {
                EditorState::new(window, cx)
                    .language("sql")
                    .line_number(true)
                    .default_value(sql)
            });
            Self { title, editor }
        })
    }
}

impl EventEmitter<PanelEvent> for SqlEditorPanel {}
impl Focusable for SqlEditorPanel {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}
impl BasePanel for SqlEditorPanel {
    fn panel_name(&self) -> &'static str {
        "SpikeSqlEditor"
    }
}
impl Panel for SqlEditorPanel {
    fn title(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        self.title.clone()
    }
}
impl Render for SqlEditorPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        Editor::new(&self.editor)
            .bordered(false)
            .h(relative(1.))
            .font_family(cx.theme().mono_font_family.clone())
            .text_size(cx.theme().mono_font_size)
    }
}

struct ResultsPanel {
    table: Entity<TableState<SyntheticRows>>,
    focus: FocusHandle,
}

impl EventEmitter<PanelEvent> for ResultsPanel {}
impl Focusable for ResultsPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}
impl BasePanel for ResultsPanel {
    fn panel_name(&self) -> &'static str {
        "SpikeResults"
    }
    fn closable(&self, _cx: &App) -> bool {
        false
    }
}
impl Panel for ResultsPanel {
    fn title(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = self.table.read(cx).delegate().rows;
        SharedString::from(format!("Results ({rows} rows)"))
    }
}
impl Render for ResultsPanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().track_focus(&self.focus).child(
            DataTable::new(&self.table)
                .stripe(true)
                .accessibility_label("Synthetic results"),
        )
    }
}

/// A plain list standing in for the database navigator (P4).
struct NavigatorPanel {
    focus: FocusHandle,
}

impl EventEmitter<PanelEvent> for NavigatorPanel {}
impl Focusable for NavigatorPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}
impl BasePanel for NavigatorPanel {
    fn panel_name(&self) -> &'static str {
        "SpikeNavigator"
    }
}
impl Panel for NavigatorPanel {
    fn title(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        "Navigator"
    }
}
impl Render for NavigatorPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let items = [
            "local-postgres (dev)",
            "  public",
            "    customers",
            "    orders",
            "    synthetic_1m",
            "  analytics",
            "staging-mysql (test)",
            "prod-pg (production)",
        ];
        v_flex()
            .size_full()
            .track_focus(&self.focus)
            .p_2()
            .gap_1()
            .text_sm()
            .font_family(cx.theme().mono_font_family.clone())
            .children(items.iter().map(|s| div().child(*s)))
    }
}

/// Bottom dock: the latest benchmark / clipboard results.
struct OutputPanel {
    lines: Rc<RefCell<Vec<String>>>,
    focus: FocusHandle,
}

impl EventEmitter<PanelEvent> for OutputPanel {}
impl Focusable for OutputPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}
impl BasePanel for OutputPanel {
    fn panel_name(&self) -> &'static str {
        "SpikeOutput"
    }
}
impl Panel for OutputPanel {
    fn title(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        "Output"
    }
}
impl Render for OutputPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let lines: Vec<String> = self.lines.borrow().iter().rev().take(6).cloned().collect();
        v_flex()
            .size_full()
            .track_focus(&self.focus)
            .p_2()
            .text_xs()
            .font_family(cx.theme().mono_font_family.clone())
            .child(
                "ctrl-shift-b scroll benchmark · ctrl-shift-y clipboard · \
                 ctrl-shift-t theme · ctrl-q quit",
            )
            .children(lines.into_iter().map(|l| div().child(l)))
    }
}

// ---------------------------------------------------------------------------------------------
// Root view and benchmark driver
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum BenchPhase {
    Idle,
    Scrolling,
    Jumping,
}

struct Spike {
    dock: Entity<DockArea>,
    _skin: Rc<DockSkin>,
    table: Entity<TableState<SyntheticRows>>,
    output: Entity<OutputPanel>,
    output_lines: Rc<RefCell<Vec<String>>>,
    metrics: Metrics,
    target_rows: usize,
    first_frame_logged: bool,
    auto: bool,
    phase: BenchPhase,
    frames_left: usize,
    direction: f32,
    last_frame: Option<Instant>,
    intervals: Vec<f64>,
    jump_seed: u64,
}

impl Spike {
    fn new(metrics: Metrics, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let auto = env_flag("NINESQL_SPIKE_AUTO");
        let target_rows = env_usize("NINESQL_SPIKE_ROWS", DEFAULT_ROWS);
        // In auto mode the table starts empty so idle RSS is measured before loading.
        let initial_rows = if auto { 0 } else { target_rows };

        let table = cx.new(|cx| {
            TableState::new(SyntheticRows { rows: initial_rows }, window, cx)
                .col_resizable(true)
                .cell_selectable(true)
        });
        let output_lines = Rc::new(RefCell::new(Vec::new()));
        let output = cx.new(|cx| OutputPanel {
            lines: output_lines.clone(),
            focus: cx.focus_handle(),
        });
        let results = cx.new(|cx| ResultsPanel {
            table: table.clone(),
            focus: cx.focus_handle(),
        });
        let navigator = cx.new(|cx| NavigatorPanel {
            focus: cx.focus_handle(),
        });
        let editor_a = SqlEditorPanel::new("query1.sql", SAMPLE_SQL, window, cx);
        let editor_b = SqlEditorPanel::new(
            "query2.sql",
            "SELECT * FROM synthetic_1m ORDER BY id;\n",
            window,
            cx,
        );

        let (dock, skin) = DockSkin::dock_area("ninesql-spike", Some(1), window, cx);
        let center = DockLayout::v_split()
            .child(
                DockLayout::tabs()
                    .panel_view(panel_handle(editor_a), cx)
                    .panel_view(panel_handle(editor_b), cx),
                Some(px(300.)),
            )
            .child(
                DockLayout::tabs().panel_view(panel_handle(results), cx),
                None,
            );
        let left = DockLayout::tabs().panel_view(panel_handle(navigator), cx);
        let bottom = DockLayout::tabs().panel_view(panel_handle(output.clone()), cx);
        dock.update(cx, |dock, cx| {
            dock.set_center(center, window, cx);
            dock.set_dock(DockPlacement::Left, left, window, cx);
            dock.set_dock_size(DockPlacement::Left, px(240.), window, cx);
            dock.set_dock(DockPlacement::Bottom, bottom, window, cx);
            dock.set_dock_size(DockPlacement::Bottom, px(140.), window, cx);
            dock.set_dock_collapsible(DockPlacement::Left, true, window, cx);
            dock.set_dock_collapsible(DockPlacement::Bottom, true, window, cx);
        });

        metrics.emit("view_built_ms", metrics.since_start_ms());

        Self {
            dock,
            _skin: skin,
            table,
            output,
            output_lines,
            metrics,
            target_rows,
            first_frame_logged: false,
            auto,
            phase: BenchPhase::Idle,
            frames_left: 0,
            direction: -1.0,
            last_frame: None,
            intervals: Vec::with_capacity(DEFAULT_SCROLL_FRAMES),
            jump_seed: 0x5EED,
        }
    }

    fn log(&mut self, line: String, cx: &mut Context<Self>) {
        self.output_lines.borrow_mut().push(line);
        self.output.update(cx, |_, cx| cx.notify());
    }

    fn on_first_frame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.metrics
            .emit("first_frame_ms", self.metrics.since_start_ms());
        self.metrics.emit_rss("rss_first_frame");
        if !self.auto {
            return;
        }
        // Auto benchmark: idle RSS, load rows, scroll, jump, clipboard, quit.
        cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(2)).await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.metrics.emit_rss("rss_idle_empty_table");
                this.load_rows(window, cx);
            });
            cx.background_executor().timer(Duration::from_secs(2)).await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.metrics.emit_rss("rss_rows_loaded");
                this.start_bench(window, cx);
            });
        })
        .detach();
    }

    fn load_rows(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rows = self.target_rows;
        let started = Instant::now();
        self.table.update(cx, |table, cx| {
            table.delegate_mut().rows = rows;
            table.refresh(cx);
        });
        let metrics = self.metrics.clone();
        window.on_next_frame(move |_, _| {
            metrics.emit(
                "rows_loaded_frame_ms",
                format!("{:.1}", started.elapsed().as_secs_f64() * 1000.0),
            );
        });
        self.metrics.emit("rows", rows);
    }

    fn start_bench(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.phase != BenchPhase::Idle {
            return;
        }
        if self.table.read(cx).delegate().rows == 0 {
            self.load_rows(window, cx);
        }
        self.phase = BenchPhase::Scrolling;
        self.frames_left = env_usize("NINESQL_SPIKE_SCROLL_FRAMES", DEFAULT_SCROLL_FRAMES);
        self.intervals.clear();
        self.last_frame = None;
        self.schedule_step(window, cx);
    }

    fn schedule_step(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let this = cx.weak_entity();
        window.on_next_frame(move |window, cx| {
            let _ = this.update(cx, |this, cx| this.bench_step(window, cx));
        });
    }

    fn bench_step(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let now = Instant::now();
        if let Some(last) = self.last_frame {
            self.intervals
                .push(now.duration_since(last).as_secs_f64() * 1000.0);
        }
        self.last_frame = Some(now);

        if self.frames_left == 0 {
            self.finish_phase(window, cx);
            return;
        }
        self.frames_left -= 1;

        match self.phase {
            BenchPhase::Scrolling => {
                let handle = self.table.read(cx).vertical_scroll_handle.clone();
                let state = handle.0.borrow();
                let mut offset = state.base_handle.offset();
                let max = state.base_handle.max_offset();
                // GPUI scroll offsets are negative when scrolled down.
                let next = offset.y + px(SCROLL_STEP_PX * self.direction);
                if next < -max.y || next > px(0.) {
                    self.direction = -self.direction;
                }
                offset.y = next.clamp(-max.y, px(0.));
                state.base_handle.set_offset(offset);
            }
            BenchPhase::Jumping => {
                self.jump_seed = mix(self.jump_seed, 1);
                let rows = self.table.read(cx).delegate().rows.max(1);
                let target = (self.jump_seed % rows as u64) as usize;
                self.table
                    .update(cx, |table, cx| table.scroll_to_row(target, cx));
            }
            BenchPhase::Idle => return,
        }
        self.table.update(cx, |_, cx| cx.notify());
        self.schedule_step(window, cx);
    }

    fn finish_phase(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = match self.phase {
            BenchPhase::Scrolling => "scroll",
            BenchPhase::Jumping => "jump",
            BenchPhase::Idle => return,
        };
        let summary = FrameStats::from_intervals(&self.intervals)
            .map(|s| s.summary())
            .unwrap_or_else(|| "no frames".to_string());
        self.metrics.emit(&format!("frames_{name}"), &summary);
        self.metrics.emit_rss(&format!("rss_after_{name}"));
        self.log(format!("{name}: {summary}"), cx);

        if self.phase == BenchPhase::Scrolling {
            self.phase = BenchPhase::Jumping;
            self.frames_left = JUMP_FRAMES;
            self.intervals.clear();
            self.last_frame = None;
            self.schedule_step(window, cx);
            return;
        }

        self.phase = BenchPhase::Idle;
        if self.auto {
            self.clipboard_round_trip(cx);
            self.metrics
                .emit("auto_done_ms", self.metrics.since_start_ms());
            if !env_flag("NINESQL_SPIKE_STAY") {
                cx.quit();
            }
        }
    }

    fn clipboard_round_trip(&mut self, cx: &mut Context<Self>) {
        let probe = format!("ninesql-spike-{}", std::process::id());
        cx.write_to_clipboard(ClipboardItem::new_string(probe.clone()));
        let read = cx.read_from_clipboard().and_then(|item| item.text());
        let ok = read.as_deref() == Some(probe.as_str());
        self.metrics.emit("clipboard_roundtrip", ok);
        self.log(format!("clipboard round trip: {ok}"), cx);
    }

    fn toggle_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mode = if cx.theme().mode.is_dark() {
            ThemeMode::Light
        } else {
            ThemeMode::Dark
        };
        Theme::change(mode, Some(window), cx);
        window.refresh();
    }
}

impl Render for Spike {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.first_frame_logged {
            self.first_frame_logged = true;
            self.metrics
                .emit("first_render_ms", self.metrics.since_start_ms());
            let this = cx.weak_entity();
            // Runs right after the first frame has been drawn.
            window.on_next_frame(move |window, cx| {
                let _ = this.update(cx, |this, cx| this.on_first_frame(window, cx));
            });
        }

        div()
            .id("spike-root")
            .size_full()
            .key_context("Spike")
            .on_action(
                cx.listener(|this, _: &ToggleTheme, window, cx| this.toggle_theme(window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &RunScrollBench, window, cx| this.start_bench(window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &ClipboardRoundTrip, _, cx| this.clipboard_round_trip(cx)),
            )
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(self.dock.clone())
    }
}

fn main() {
    let start = Instant::now();
    let metrics = Metrics::from_env(start);
    let app = gpui_kit::application().with_assets(Assets);

    app.run(move |cx| {
        gpui_kit::init(cx);
        if std::env::var("NINESQL_SPIKE_THEME").is_ok_and(|v| v == "dark") {
            Theme::change(ThemeMode::Dark, None, cx);
        }
        cx.bind_keys([
            KeyBinding::new("ctrl-q", Quit, None),
            KeyBinding::new("ctrl-shift-t", ToggleTheme, None),
            KeyBinding::new("ctrl-shift-b", RunScrollBench, None),
            KeyBinding::new("ctrl-shift-y", ClipboardRoundTrip, None),
        ]);
        cx.on_action(|_: &Quit, cx| cx.quit());
        metrics.emit("app_init_ms", metrics.since_start_ms());

        let bounds = Bounds::centered(None, size(px(1280.), px(800.)), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some("NineSQL - GPUI spike (P0-A)".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let metrics = metrics.clone();
        if let Err(err) = gpui_kit::open_window(options, cx, |window, cx| {
            cx.new(|cx| Spike::new(metrics, window, cx))
        }) {
            let _ = writeln!(std::io::stderr(), "spike: cannot open a window: {err:#}");
            cx.quit();
        }
        cx.activate(true);
    });
}

#[cfg(test)]
mod tests {
    // Explicit imports: the `gpui_kit::*` glob would shadow `#[test]` with GPUI's macro.
    use super::{FrameStats, STATUS, synthetic_cell};

    #[test]
    fn cells_are_deterministic() {
        assert_eq!(synthetic_cell(0, 0), "1");
        assert_eq!(synthetic_cell(999_999, 0), "1000000");
        assert_eq!(synthetic_cell(42, 3), synthetic_cell(42, 3));
        assert_eq!(synthetic_cell(12_345, 1).len(), 18);
        assert!(STATUS.contains(&synthetic_cell(7, 6).as_str()));
        assert_eq!(synthetic_cell(3, 7), "NULL");
    }

    #[test]
    fn frame_stats_percentiles() {
        let v: Vec<f64> = (1..=100).map(f64::from).collect();
        let s = FrameStats::from_intervals(&v);
        assert!(s.is_some());
        if let Some(s) = s {
            assert_eq!(s.count, 100);
            assert!((s.p50 - 50.0).abs() <= 1.0);
            assert!((s.p99 - 99.0).abs() <= 1.0);
            assert!((s.max - 100.0).abs() < f64::EPSILON);
        }
        assert!(FrameStats::from_intervals(&[]).is_none());
    }
}
