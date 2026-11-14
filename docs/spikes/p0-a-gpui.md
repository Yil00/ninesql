# P0-A GPUI spike: results and go / no-go

- **Task**: ROADMAP P0-A ([ADR-0001](../adr/0001-native-stack-rust-gpui.md), [ADR-0002](../adr/0002-licensing-and-dependency-policy.md))
- **Date**: 2026-11-14
- **Code**: [`crates/ninesql-ui/examples/spike.rs`](../../crates/ninesql-ui/examples/spike.rs), measurement script [`spike-measure.sh`](../../crates/ninesql-ui/examples/spike-measure.sh), CI [`gpui-spike.yml`](../../.github/workflows/gpui-spike.yml)
- **Verdict**: **GO**, with the workarounds listed below (X11 IME is the main risk).

## Pinned revisions
| Component | Pin | Notes |
|---|---|---|
| `gpui-kit` (facade: GPUI + `gpui-base` + `gpui-component` + assets + platform) | `=0.9.1` on crates.io | Same source as `longbridge/gpui-component` tag `v0.9.1`, commit `39ab849a93b5f15507b969164d49891db8d5b470` |
| `gpui-component` | `0.9.1` (through `gpui-kit`) | Apache-2.0 |
| GPUI | `gpui-pre =0.3.12` (exact pin made by `gpui-kit`) | crates.io snapshot of `zed-industries/zed@d2a4fb6`, Apache-2.0 |

Why crates.io and not a `zed-industries/zed` git revision: since its 0.8 line, `gpui-component` no longer depends on Zed's git repository; it depends on the `gpui-pre-*` crates, exact-pinned snapshots of Zed's GPUI crates. A git pin of `zed` would produce a second, incompatible `gpui` crate next to the one `gpui-component` uses. ADR-0001 §3 allows the switch to crates.io releases without a new ADR. Two follow-ups for the security review:
- The `gpui-pre-*` crates are published by the `gpui-component` maintainer, not by the `zed-industries` crates.io team (who publish `gpui` 0.2.x, too old for `gpui-component`). Supply-chain trust moves from "Zed's git repository" to "a third-party snapshot of it"; Cargo.lock checksums pin the exact bytes.
- The two `allow-git` entries in `deny.toml` are now unused (cargo-deny warns `unmatched-source`, not an error); they are kept in case a later bump goes back to git.

Dependency graph: about 670 crates for `ninesql-ui` with the spike on Linux (810 in `Cargo.lock` across all targets). `cargo deny --locked check`: licenses, bans and sources pass unchanged (no Zed GPL crate: `editor`, `language`, `project`, `workspace` are absent). Advisories needed:
- `cargo update` of `time` (0.3.45 to 0.3.47, RUSTSEC-2026-0009) and `zbus_xml` (5.1.0 to 5.2.1, drops `quick-xml` 0.38: RUSTSEC-2026-0194 / -0195). Both require Rust 1.87 / 1.88, above the workspace `rust-version = "1.85"`; GPUI itself uses let-chains (Rust 1.88), so the workspace MSRV should move to at least 1.88.
- Eight "unmaintained" notices with no maintained release in the pinned pair, ignored one by one with a reason in `deny.toml` (`instant`, `paste`, `rustls-pemfile` (wasm only), `rustybuzz`, `ttf-parser`, `zbus_names`, `zvariant`, `zvariant_derive`). No known vulnerability is ignored.

Network (I2): GPUI links `gpui-pre-http-client` (a trait used for remote images); no HTTP client implementation is linked on Linux (`gpui-pre-reqwest` is only pulled for the wasm asset loader). D-Bus is used for XDG portals, notifications and AT-SPI; no network traffic was observed or expected.

## Build
- System packages: the list in [docs/10-supported-platforms.md](../10-supported-platforms.md) builds on Ubuntu 24.04. The binary links only `libxcb`, `libxcb-xkb`, `libxkbcommon`, `libxkbcommon-x11` (Vulkan, Wayland, fontconfig are loaded at run time, FreeType is built from source), so the strictly needed `-dev` packages are `pkg-config`, `libxkbcommon-dev`, `libxkbcommon-x11-dev`, `libxcb1-dev`; the rest of the list can be trimmed after a CI run confirms it.
- Run time: `libvulkan1` plus a Vulkan driver (`mesa-vulkan-drivers` covers radv, anv, nouveau / NVK and lavapipe). GPUI now renders through `wgpu` (Vulkan; GL fallback is compiled in).
- Cold build of the GPUI graph on 4 shared vCPUs (`-j 3`): debug 3 min 44 s, release (thin LTO, `codegen-units = 1`) 10 min 43 s. Release binary: 60 MB (debug info stripped). `target/` reached about 9 GB with both profiles.

## What the spike shows
- A 1280 x 800 window, a dock area (`gpui-component` `DockArea` + `DockSkin`): navigator on the left, output at the bottom, both collapsible and resizable; a center split with a tab group of two SQL editors over a results tab group.
- SQL editor: `gpui-component` `EditorState` with the `tree-sitter-sql` feature (grammar `tree-sitter-sequel`, MIT): live highlighting, line numbers, typing, select-all, copy, paste.
- Results: `DataTable` over a `TableDelegate` that reports 1 000 000 rows and computes each cell from `(row, column)` only when it is drawn (I6). Cell selection, keyboard navigation (arrows, Page Up / Down, Home / End), column resize, stripes, a11y labels per cell.
- Light and dark themes (`ctrl-shift-t`), scripted benchmark (`NINESQL_SPIKE_AUTO=1`), clipboard probe.

Screenshots were taken in the cloud sandbox (Xvfb, software rendering) and attached to the PR, not committed. Screenshots on a real GPU are left to a local run (`SPIKE_SCREENSHOT=1`).

## Measurements
Cloud sandbox, no GPU: Mesa **llvmpipe / lavapipe software rendering** (LLVM 20, Vulkan 1.4), 4 shared vCPUs, release build, median of 3 runs of `spike-measure.sh`. X11 = Xvfb 1600 x 1000. Wayland = weston 13 (pixman renderer) nested in Xvfb, because weston's headless backend exposes no `wl_seat` and GPUI panics without one.

**Frame times below are software-rendered and NOT representative of a real GPU.** They show the frame pacing of a CPU rasterizer, quantized on GPUI's 16 ms frame clock on X11.

| Metric | X11 (Xvfb) | Wayland (nested weston) |
|---|---|---|
| main() to platform ready | 21 ms | 9 ms |
| Cold start to first frame (from `main`) | 184 ms (172–213) | 202 ms (187–215) |
| RSS at first frame | 158 MiB | 159 MiB |
| RSS idle, empty table, 2 s after first frame | 204 MiB | 209 MiB |
| RSS with 1 M rows loaded | 204 MiB (+0) | 209 MiB (+0) |
| RSS after 600 frames of scrolling | 217 MiB | 221 MiB |
| RSS after 120 random jumps across 1 M rows | 217 MiB | 221 MiB |
| Scroll frame interval mean / p50 / p95 / p99 / max (software) | 49 / 48 / 64 / 65 / 80 ms | 54 / 53 / 66 / 75 / 94 ms |
| Scroll mean FPS (software) | 20.4 | 18.5 |
| Random-jump frame interval mean / p95 / max (software) | 48 / 48 / 64 ms | 54 / 65 / 73 ms |

Reading:
- Loading 1 M rows costs no memory: the table only builds the visible rows; RSS grows by about 13 MiB while scrolling (glyph atlas and text layout caches) and then stays flat.
- Most of the idle RSS is the software rasterizer (llvmpipe JIT, CPU framebuffers) and the font stack; a real GPU driver changes this number. The local run on a real GPU is needed for the ROADMAP's acceptance numbers.
- Debug builds are about 3x slower per frame (137 ms mean under llvmpipe): measure release builds only.

To reproduce on a real GPU, from the repository root: `crates/ninesql-ui/examples/spike-measure.sh` in a Wayland session, then `SPIKE_SESSION=x11` for XWayland or a native X11 session. Add `SPIKE_DROP_CACHES=1` (sudo) for a cold-cache start.

## Capabilities for phases 1–5
| Need (phase) | GPUI / gpui-component at this pin | Verdict and workaround |
|---|---|---|
| Window, tabs, docks, splits, persisted layout (P1) | `DockArea` with tab groups, splits, collapsible docks, drag-and-drop of panels, `dump` / `load` of the layout | Go |
| Command palette, actions, key bindings (P1) | GPUI `Action` + `KeyBinding` + key contexts; `gpui-component` has a `command` module | Go |
| Dialogs, forms, notifications (P1 connection dialog) | `dialog`, `form`, `notification`, `select`, `switch`, `input` (masked input exists, but per AGENTS.md we never show a password) | Go |
| SQL editor (P3) | `EditorState`: rope buffer, tree-sitter highlighting and folding, line numbers, soft wrap, search, LSP-style completion / hover / code-action / diagnostics providers, indent guides | Go. Completion and diagnostics are fed by our own providers (no LSP server needed). Statement splitting and "run statement under cursor" stay ours (`ninesql-editor`). Fallback in ADR-0001 §5 not needed now |
| Results grid (P3, P5) | `DataTable`: virtual rows (uniform height) and virtual columns, fixed columns, resize, move, sort callbacks, row / column / cell selection, context menu, keyboard navigation, a11y labels | Go. Missing for us: multi-cell range selection (`TableSelection` is one row, one column or one cell), ctrl-home / ctrl-end (not bound), in-cell editing (P5) and variable-height rows; build them in `ninesql-grid` (own delegate + overlay input), or contribute upstream |
| Clipboard (P3) | X11: copy and paste work both ways with other apps (checked with `xclip`), in-process round trip OK. Wayland: copy and paste both ways with other apps OK (checked with `wl-copy` / `wl-paste`); a read issued in the same tick as our own write returns the previous content | Go. Never read back our own clipboard write synchronously; keep the copied value in memory |
| IME (P3) | Wayland: `text-input-v3` (UTF-8). X11: XIM through the `zed-xim` / `xim-ctext` crates | **Risk on X11**: with ibus (m17n `latn-post`, UTF-8 locale) the app **panics** on the first preedit containing a non-ASCII character: `xim-ctext` decodes unescaped COMPOUND_TEXT as UTF-8 instead of Latin-1 and returns an error for GB2312 / KSC escapes, and `zed-xim` calls `expect` on it. Workaround: patch `xim-ctext` / `zed-xim` (MIT) and carry it with `[patch.crates-io]` until upstream merges; test CJK engines (ibus-pinyin, mozc, hangul) locally. Wayland IME could not be tested (weston implements only `text-input-v1`): test on GNOME / KDE locally |
| Drag-and-drop (P4 navigator, P5) | Internal drag-and-drop (dock panels, table columns) works. External drops of files are received on X11 (XDND) and Wayland. Drag-out (`start_external_drag`) exists only on Wayland and only for file lists; nothing for X11 or for text payloads (code read, not exercised in the sandbox) | Go for internal DnD ("drag a table into the editor" is internal). Drag-out of query results to other apps: not in v1, or write a temporary export file and drag it on Wayland |
| Accessibility (all) | GPUI now links AccessKit (`accesskit_unix`, AT-SPI over D-Bus); `gpui-component` exposes labels for tables, cells, inputs | Partial: not verified with Orca in the sandbox. Keep full keyboard navigation; test Orca locally before P6 |
| Themes and contrast | Light / dark themes switch at run time | Go, but the default dark theme draws table headers in low-contrast grey: check contrast (WCAG AA) and override theme colors in P1 |
| Wayland decorations | No server-side decorations under weston (no title bar); GPUI / `gpui-component` provide client-side title bars (`TitleBar`) | Use `gpui-component`'s `TitleBar` with client-side decorations on Wayland in P1 |
| Robustness | GPUI panics when the Wayland compositor has no `wl_seat` (headless) and on the X11 IME error above | Report upstream; not a blocker for desktop sessions |
| Tests without display | GPUI `TestAppContext` and `gpui-kit` `test-support` | Go for view-model tests; CI runs a software-rendered smoke test of the spike under Xvfb |

## Follow-ups
1. Run `spike-measure.sh` on a real GPU (Wayland and X11) and record the numbers in the PR (ROADMAP acceptance).
2. File upstream issues: `xim-ctext` / `zed-xim` panic on Latin-1 / CJK compound text; GPUI panic without `wl_seat`.
3. Raise the workspace `rust-version` to 1.88 (GPUI let-chains; `time` 0.3.47).
4. Trim `docs/10-supported-platforms.md` build packages after a green CI run; add `libvulkan1` + `mesa-vulkan-drivers` as run-time dependencies of the `.deb`.
5. Ubuntu 22.04 and Debian 12: not tested here (the sandbox is Ubuntu 24.04). Check that the binary builds and that `wgpu` finds a Vulkan adapter with their Mesa versions before keeping them as targets.
