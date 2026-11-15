# ninesql-grid

The results grid of NineSQL (ROADMAP P3-B; F-DATA-01, F-SQL-04): a virtualized GPUI view over
Arrow batches, under a memory budget (ADR-0004, invariant I6). It is a reusable component: it does
not know about sessions or drivers, only about the result types of `ninesql-driver-api`
(`ResultItem`, `RecordBatch`, `FetchBudget`, the `ninesql.type` / `ninesql.truncated` field
metadata), and talks to whoever runs the statements through a small adapter.

## Layers

| Module | GPUI | What |
|---|---|---|
| `format` | no | Type-aware cell text: NULL, numbers right-aligned, dates / times / timestamps (UTC instants labelled `UTC`), binary as a hex preview, JSON on one line, booleans, long text cut with `…`, control and bidirectional characters made visible |
| `model` | no | One result set: batches, row lookup (binary search over batch offsets), paging state, bytes, truncated columns |
| `selection` | no | Cell / row / column / all selection in display coordinates, column order |
| `copy` | no | TSV / CSV copy, RFC 4180 quoting, bounded at 64 MiB |
| `execution` | no | Everything one execution produced: result tabs, update counts, notices, errors, the execution memory budget |
| `source` | no | The adapter: `ResultSource` (implemented by the execution service) and `ExecutionEvent` |
| `view` | yes | `ResultsView` (tabs, panes, banner, fetch controls, actions) and `GridDelegate` (`gpui-component` `DataTable`) |
| `actions` | yes | The grid actions and their default key bindings (`GRID_ACTIONS`) |

## Wiring (for `ninesql-core` and `ninesql-ui`)

1. Create the view: `cx.new(|cx| ResultsView::new(GridBudget::from_fetch_budget(&budget), cx))`.
2. Give it a source: `view.set_source(Rc::new(my_source))`, where `my_source: impl ResultSource`
   (`fetch(FetchRequest)`, `release(statement)`; both return at once).
3. Forward the execution service's events on the foreground thread:
   `view.update_in(cx, |v, window, cx| v.apply(event, window, cx))`.
4. Install the key bindings: `ninesql_grid::init(cx)` after `gpui_kit::init(cx)`, or merge
   `actions::GRID_ACTIONS` into the shell's keymap (overrides) and command palette.

The full contract (first page delivered unprompted, `StatementFinished` versus
`StatementClosed`, `release`) is in the rustdoc of `source`.

## Memory policy (I6)

- Every `ResultStream` stops at its own `FetchBudget::max_bytes` with `Truncated`. A script runs
  one stream per statement, so the grid keeps its own account **per execution** (one editor tab):
  `GridBudget::max_bytes`, over every kept batch (`get_array_memory_size`, the stream's measure)
  and every notice.
- **Retain, never evict.** Streams are forward-only: a dropped batch could only come back by
  running the statement again, which may not be repeatable and is never done silently. So the grid
  keeps every batch it accepted and stops accepting at the budget: the batch that would cross it
  is not kept, the result shows "Truncated: memory budget reached", and the grid asks the source
  to release the stream (server cursor closed).
- Nothing else grows with the result: cells are formatted only when drawn or copied (no text
  cache), copy is bounded, notices are bounded (10 000 by default), document result sets are
  counted, not kept, until the document viewer exists (P5-A).
- "Fetch all" asks for every remaining row and ends at the end of the result or at the first
  budget reached; it can never load more than the budget, so it needs no confirmation.

## Truncated values (I8)

A column flagged `ninesql.truncated` in any batch is marked for the whole result: a "cut" badge in
its header, `ColumnMeta::editable() == false`, a banner, and a copy that includes such a column
says that some values were cut.

## Actions

All in the `ResultGrid` key context, namespace `ninesql_grid` (the shell's `ninesql::` registry is
unchanged):

| Action | Default key | Palette label |
|---|---|---|
| `CopySelection` | `ctrl-c` | Copy selected cells |
| `CopySelectionWithHeaders` | `ctrl-shift-c` | Copy selected cells with column names |
| `CopySelectionAsCsv` | `ctrl-alt-c` | Copy selected cells as CSV |
| `SelectAllCells` | `ctrl-a` | Select all loaded rows |
| `ExtendSelectionUp` / `Down` / `Left` / `Right` | `shift-up` / `shift-down` / `shift-left` / `shift-right` | Extend selection … |
| `FetchNextPage` | `alt-pagedown` | Fetch next page of rows |
| `FetchAllRows` | `alt-end` | Fetch all rows (up to the memory budget) |
| `AutoSizeColumn` | `alt-r` | Fit column width to content |
| `AutoSizeAllColumns` | `alt-shift-r` | Fit all column widths to content |
| `NextResultTab` / `PreviousResultTab` | `alt-right` / `alt-left` | Next / previous result tab |

Arrow keys, Page Up / Down, Home / End and Escape are the `DataTable`'s own cell navigation;
column resize (drag, double-click to fit) and reorder (drag the header) are the `DataTable`'s too.

## Tests and benchmarks

- `cargo test -p ninesql-grid --all-features`: unit tests of every model, `TestAppContext` tests of
  the view (actions through their key bindings, clipboard, paging, budget, truncated columns, one
  million rows with only the visible ones built), and `tests/headless_perf.rs`, bounded checks on
  one million rows that run in CI.
- `cargo bench -p ninesql-grid`: headless numbers on one million rows of seven typed columns in
  200-row batches: ingestion with memory accounting, visible window at random positions, copy of
  10 000 / 100 000 / 1 000 000 rows, memory account reads. Without `--bench` (for example
  `cargo test --benches`) it runs a quick 20 000-row pass.

## Local measurement protocol (maintainer, real GPU)

Cloud sandboxes have no display and no GPU; these numbers are taken locally, on the reference
machine, with a release build, and attached to the P3-E PR. They need the grid wired into
`ninesql-ui` and the P3-C execution service.

1. Prepare: `make dev-up` (or `docker compose -f dev/compose.yaml up -d postgresql`), check that
   `big_events` has 1 000 000 rows in the "nine" dataset, build with `cargo build --release -p
   ninesql-ui`. Close other GPU-heavy applications; note CPU, GPU, driver, compositor (Wayland or
   X11), display scale and refresh rate.
2. Default budget: leave `FetchBudget` at its defaults (200-row batches, 256 MiB).
3. **Time to first rows**: run `SELECT * FROM big_events` from the editor 10 times (fresh tab each
   time). Record the time from the execute action to the first frame showing rows, by screen
   recording at 120 fps or more; with `RUST_LOG=ninesql_grid=debug`, the `first rows` event marks
   when the first batch reached the grid (the frame follows). Report median and max. Target:
   under 300 ms (phase 3 exit criterion).
4. **Scroll frame times**: with the first page shown, press "Fetch all" and wait for the banner or
   the end; then scroll continuously for 10 s (held Page Down, then a fast scrollbar drag from top
   to bottom and back, then 100 random jumps with Home / End and the scrollbar). Record
   frame intervals (`MANGOHUD=1` or the compositor's frame timing; GPUI's own frame timing where
   available). Report mean, p50, p95, p99, max and dropped frames at the display's refresh rate.
   Target: no dropped frames at 60 Hz.
5. **Memory at budget**: before the query, after the first page, after "Fetch all" reached the
   budget, after 2 minutes of scrolling, and after closing the result tab: record RSS
   (`ps -o rss= -p $(pidof ninesql)`). The result's share (RSS after "Fetch all" minus RSS after
   the first page) must stay at or under the budget plus a small constant; the closed tab must give
   it back. The status line shows the grid's own account (`x MiB of 256.0 MiB`).
6. **Cancel**: start `SELECT * FROM big_events` with "Fetch all", cancel after about 1 s: the grid
   shows "Stopped", the session is usable for the next statement.
7. **Copy**: select all rows (Ctrl+A) and copy (Ctrl+C) after "Fetch all"; record the time and
   check the status line (it stops at 64 MiB and says so).
8. Repeat 3–5 with `mysql` (`big_events` on MySQL 8.4) and with a SQLite copy of the dataset.

Report the numbers with `cargo bench -p ninesql-grid` output from the same machine.
